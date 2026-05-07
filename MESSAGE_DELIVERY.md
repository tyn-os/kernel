# Message delivery / scheduler-wake liveness

After commit `6704b69`, OTP 27 boots at **89%**. The remaining ~11% were
**scheduler-progress stalls**: schedulers sat in `futex_wait` on SSI
events, got "rescued" by the watchdog, and stayed stuck.

**Resolved (this session): the watchdog rescue was a no-op.** Fix in
the B1 section below. Boot reliability is now **64/64 = 100%** on
the 30-second KVM trial.

**Still open: Bandit's stall.** Same accept-then-fcntl-then-silence
pattern, even with the watchdog fix. So it's a different bug class
than the boot stalls, not the unified-cause hypothesis I'd
expected.

This is not a data-corruption bug — those were all fixed in
`6704b69`. The remaining issue is a liveness/scheduling bug that
only affects Bandit's `DynamicSupervisor` → `Handler` path.

## Symptom catalog

### A. Boot-time SSI-event stalls (~11%)

```
[wait]    tid=4 addr=0x566642d0 expect=0xffffffff cur=0xffffffff
[wake_call] addr=0x566642d0 count=1 val=0x0
[wake]    tid=4 addr=0x566642d0
[wake_call] addr=0x566642d0 count=1 val=0x0
[pending_wake] addr=0x566642d0
[watchdog] woke tid=4 addr=0x566642d0 old_val=0xffffffff cur=0x0
qemu-system-x86_64: terminating on signal 15 (timeout)
```

A scheduler thread (typically tid 4 or 5 — a regular ERTS scheduler)
sits in `futex_wait` on the address of its `ErtsSchedulerSleepInfo`
event. The futex value `0xffffffff` is ERTS's `OFF_WAITER` sentinel —
the scheduler is genuinely waiting. The first `wake_call` fires and
delivers (we see `[wake] tid=N`). A second `wake_call` for the same
address arrives slightly later, finds no blocker, and goes into
`pending_wake`. Eventually the watchdog (1 Hz timer) finds the
thread still `Blocked` on the same address and force-wakes it.

### B. Bandit handler stall

```
[accept] connection established!
[accept] returning new_fd=502
[sock-sc] t4 nr=72 fd=502 a1=0x3 a2=0x0     # fcntl F_GETFL
[sock-sc] t4 nr=72 fd=502 a1=0x4 a2=0x8800  # fcntl F_SETFL = O_NONBLOCK|O_CLOEXEC
                                            # ... no further syscalls on fd 502 ...
```

After `gen_tcp:accept` and `gen_tcp:controlling_process`, the
`Acceptor` calls `DynamicSupervisor.start_child(sup_pid, child_spec)`
to spawn a `Handler` GenServer, then sends it
`{:thousand_island_ready, ...}`. The Handler's `handle_info` would
call `inet:peername` (a syscall) and then `gen_tcp:recv`. We see
**neither**. The Handler process is logically runnable but never
runs.

A manual reproduction with **raw `spawn(fun…end)` + `controlling_process`
+ `! {sock,S}`** **does** work — `curl` returns "Hi". So the
primitive scheduling/messaging works. Something about the GenServer
+ DynamicSupervisor path triggers the stall.

## ERTS wake-delivery protocol (relevant pieces)

ERTS's `wake_scheduler` chain in `erl_process.c`:

```
wake_scheduler(rq) → ssi_wake(rq->scheduler->ssi)
ssi_wake(ssi):
    flags = ssi_flags_set_wake(ssi)         // CAS clears SLEEPING|WAITING
    erts_sched_finish_poke(ssi, flags)      // dispatch by sleep type:
        case POLL_SLEEPING:  erts_check_io_interrupt(ssi->psi, 1)
        case TSE_SLEEPING:   erts_tse_set(ssi->event)         ← futex_wake
        case POLL|TSE:       both
        case 0:              break (no wake — scheduler isn't actually asleep)
```

The waiter (`erts_thr_progress_wait` / scheduler block path) does:

1. Set `SLEEPING` in flags.
2. `prepare_wait` (memory barriers).
3. CAS to add `TSE_SLEEPING`. If that fails because flags no longer
   include `SLEEPING` (a wake already cleared them), break — don't
   sleep.
4. `erts_tse_wait(event)` — futex_wait on the SSI event.

Our `pending_wake` mechanism handles the race where the wake fires
between step 3 and step 4: the wake records a pending entry, the
waiter's `futex_wait` consumes it on entry and returns 0
immediately.

## What's confirmed working

- `futex_wait` / `futex_wake` themselves: a wake that arrives while
  a thread is genuinely `Blocked` finds it and queues it correctly
  (we see `[wake] tid=N` in successful runs).
- `pending_wake` (one-shot) handles wake-before-wait races. Multi-
  shot was tested and didn't measurably change reliability — the
  one-shot version is sufficient for the scenarios it covers.
- `gen_tcp` primitives (`listen`/`accept`/`setopts({active,once})`/
  `controlling_process`/`{tcp,S,Data}` delivery to non-acceptor
  process / `send` / `close`) all verified by manual ThousandIsland-
  style flow.
- `erlang:!` and `receive` between two processes spawned via
  `spawn(fun()…end)`.

## What's broken

### B1. The watchdog "wake" didn't actually reschedule (FIXED)

`src/sched.rs:780 watchdog_wake` was setting `state=Ready` and
clearing `futex_addr`, but **not pushing the thread onto its CPU's
run queue**:

```rust
// before fix:
thread.state = State::Ready;
let addr = thread.futex_addr;
thread.futex_addr = 0;
// ... and that's it. The thread is NOT pushed onto its CPU's queue.
```

`cpu_idle_loop` only pulls from the queue:

```rust
let next = unsafe { CPU_QUEUES[cpu].queue.pop_front() };
```

So a thread that the watchdog "rescued" stayed in `state=Ready`
but sat orphaned outside any queue. The next real `futex_wake` for
it checked `state == Blocked`, saw `Ready`, and skipped it (because
the wake-iteration only matches `Blocked` threads). Pending-wake
got recorded but the thread wasn't going to enter `futex_wait`
again at the same address.

The original comment ("Adding to queue would cause dual
scheduling") was real: if both watchdog and a real `futex_wake`
queue the same thread, the idle loop pops it twice and the second
pop runs a thread that's already running on another CPU.

**Fix:** queue from the watchdog **iff** not already queued, with
a small `contains` check on push (queue depth is ~20, so it's
cheap). Also IPI the target CPU so it wakes from `hlt` if idle:

```rust
let _qlock = CPU_QUEUE_LOCKS[target_cpu].lock();
if !CPU_QUEUES[target_cpu].queue.iter().any(|&t| t == tid) {
    CPU_QUEUES[target_cpu].queue.push_back(tid);
}
if target_cpu != current_cpu() as usize {
    crate::apic::send_ipi(target_cpu as u8);
}
```

**Result:** 32-trial test = 32/32 OK. 64-trial confirmation = 64/64
OK (was 57/64 before). Zero timeouts.

### B2. Why does Bandit's handler not run, while raw `spawn` works? (STILL OPEN)

After the B1 fix, boot is 100% reliable but **Bandit still stalls
exactly the same way**. So the unified-cause hypothesis (B1 also
explains B2) is wrong. The watchdog isn't even firing for Bandit
runs in interesting ways.

Both paths end with ERTS's `erts_proc_notify_new_message` (or
equivalent), which marks the target runnable and pokes the owning
scheduler. The difference is in process-creation:
`DynamicSupervisor.start_child` is a synchronous `gen_server:call`
that itself goes through the run-queue/scheduler chain. Raw
`spawn` is direct — no intermediate gen_server hop.

The "fcntl + silence" pattern: the fcntl is inside
`gen_tcp:controlling_process`'s `inet:setopts(S, [{active, false}])`.
After that, the Acceptor:
1. Calls `port_connect`/`port_set_owner` to transfer the inet port
2. Drains pending `{tcp,Socket,_}` from its mailbox
3. Sends `{:thousand_island_ready, ...}` to the Handler GenServer
4. Re-enters the accept loop (calls `gen_tcp:accept` on the listener)

Steps 1–3 are in-memory ERTS operations (no syscalls). Step 4
should produce another accept call on fd 501 — but we don't see
it. So the Acceptor is stuck somewhere in steps 1–3.

Possible causes:
- **port_connect blocking**: maybe transferring port ownership
  goes through a sync mechanism we miss.
- **mailbox drain selective-receive blocking**: if the drain uses
  a `receive {tcp,Socket,_} -> ... after 0 -> end` pattern, and
  something's wrong with our after-0 timeout, it could block.
- **gen_server:call earlier returned an error or stopped**:
  the start_child might have failed silently and the Acceptor's
  result-handling path waits on a different futex.

Confirming requires patching ThousandIsland or ERTS source with
markers (we tried earlier; Elixir compile is broken on the build
server). Alternative: add kernel-side instrumentation that dumps
all thread states + futex_addrs every 5 seconds during a Bandit
run, look for the Acceptor and the Handler in their respective
states.

## Mitigations and dead ends

| Attempt | Effect |
|---|---|
| One-shot `pending_wake` (already in tree before this branch) | Required for the wake-before-wait race; doesn't cover post-wake re-block |
| Multi-shot `pending_wake` (counter per address) | No measurable improvement on either symptom; reverted |
| Watchdog (1 Hz, force-wakes blocked threads on value change) | Reduces obvious cases of value-changed-without-wake; **but** see B1 — may not actually reschedule |
| FXSAVE/FXRSTOR + RFLAGS preservation + mmap zero | Eliminated all data-corruption failures; orthogonal to liveness |

## Suggested next investigations

In rough order of expected payoff:

1. ~~**Fix B1**~~ DONE — boot reliability went from 89% to 100%.
2. **Periodic thread-state dump**: every ~5 seconds, log all
   thread states + futex_addrs + queue positions. During a Bandit
   stall this would show whether the Acceptor and Handler are
   `Blocked` (and on what), `Ready` (and queued where), or
   `Running` (and on which CPU).
3. **Trace `inet_drv` port_connect/port_set_owner**: these are
   in-memory ERTS but may interact with a global port table that
   has its own locking. If the lock is held by a stalled
   process, this would block.
4. **Trace selective-receive `after 0`**: the controlling_process
   mailbox-drain uses this pattern. If our scheduler doesn't
   correctly handle `after 0` as "non-blocking", the Acceptor
   could effectively spin or block.
5. **Audit IPI delivery**: `apic::send_ipi` is fire-and-forget.
   If the target CPU's APIC is in a state where IPIs are masked,
   we don't notice. Logging IPI destination + delivery
   confirmation would catch this.

## Quick experiment template

To rule a hypothesis in/out fast: run 32 boots, count timeouts,
inspect their last-30 lines. If a hypothesis predicts a specific
log signature, grep for it across all timeout traces.

```bash
ssh build-server '
for i in $(seq 1 32); do
  timeout 30 qemu-system-x86_64 -kernel /tmp/tyn -serial stdio \
    -display none -no-reboot -smp 2 -m 2G -enable-kvm \
    -netdev user,id=net0,hostfwd=tcp::5566-:8080 \
    -device virtio-net-pci,netdev=net0,disable-legacy=on,disable-modern=off \
    -machine q35 > /tmp/r$i.log 2>&1
done
# Then categorize: ok / load / pf / timeout
'
```

## Adjacent context

- `src/sched.rs:780 watchdog_wake` — likely-broken rescue path (B1)
- `src/sched.rs:253 cpu_idle_loop` — only checks queue, never `state`
- `src/sched.rs:708 futex_wake` — queues correctly + IPIs target CPU
- `src/sched.rs:174 pending_wake_insert` — one-shot flat array
- `src/sched.rs:189 pending_wake_consume` — first matching wait wins
