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

### B2.5. Narrowing: `receive after N` doesn't fire after Bandit starts (NEW)

A diagnostic spawn placed right after `Bandit.start_link` reveals
the real shape of the bug. With this eval:

```erlang
{ok,_} = 'Elixir.Bandit':start_link(...),
spawn(fun() ->
    erlang:display(spawnA_alive),
    receive after 5000 -> ok end,
    erlang:display(spawnA_5s)
end).
```

The serial output shows:

```
post_bandit_a         ← start_link returned
{pa,<0.387.0>}        ← spawn returned a Pid
post_bandit_b         ← parent continued
spawnA_alive          ← spawned process ran its first display
                      ← (and now nothing — for 30+ seconds)
```

**The spawned process runs.** It prints its first display
immediately. Then it enters `receive after 5000 -> ok end` and
**never returns from the receive.** The 5-second timeout never
fires.

The same `receive after N -> ...` pattern works in non-Bandit
evals (the handler timeout in the manual ThousandIsland demo
fires correctly when curl doesn't connect). So the bug isn't in
basic timer functionality — it's specifically triggered by
something Bandit's supervision tree does to the scheduler state.

This narrows the suspected mechanism dramatically. ERTS's
`receive after N` is implemented via the internal timer wheel
which is advanced by schedulers as part of their normal loop.
If a scheduler stops advancing its timer wheel — because it's
permanently in a sleep state, or because its wake-from-sleep
path doesn't fire timer processing — every `after N` attached to
that scheduler stalls.

Bandit's stall (B2) and this `after N` stall are now strongly
correlated. The Handler that should receive
`{:thousand_island_ready, ...}` may itself be in
`receive ... after N -> ...` waiting for a default timeout, and
the timer never fires.

### B2.6. `sys_futex` ignored the timeout argument (FIXED)

Inspecting ERTS's `ethr_event.c` revealed `wait__()` calls
`futex(addr, FUTEX_WAIT, OFF_WAITER, &timespec)` — passing a
`struct timespec*` as the 4th argument. Our `sys_futex` only took
3 args and silently dropped the timeout, turning every
`ethr_event_twait(N)` into an indefinite wait. This breaks
ERTS's scheduler-level timer-aware sleep — the path that should
fire `receive after N` and `gen_server:call` timeouts.

Fix landed in this branch:

- `sys_futex` now takes `timeout_ptr` and parses the timespec.
- `futex_wait_until(addr, val, deadline)` records the deadline
  on the Thread struct (`wait_deadline_ns`) before context-
  switching out.
- The watchdog (now running every timer tick = 10 ms instead of
  every second) checks `wait_deadline_ns` against `monotonic_ns()`
  on every Blocked thread and rescues those whose deadline has
  passed, with the same queue-and-IPI path B1 added.
- On resume, `futex_wait_until` checks `wait_timed_out` and
  returns `-110` (`-ETIMEDOUT`), which `ethr_event.c` translates
  to `ETIMEDOUT` for `ethr_event_twait`.

Boot reliability with this change: 32/32 OK (no regression).
Without Bandit, `spawn(fun() -> receive after 5000 -> ok end ...)`
fires the 5-second timer correctly. So the futex-timeout bug
was real and the kernel-side fix is correct.

**But: with Bandit, `receive after N` still doesn't fire.** So
the futex timeout was a real bug we hadn't fixed, and B2.5
was a real symptom — but Bandit-specific something keeps the
timer wheel from advancing on whichever scheduler holds the
probe's process. The probe `spawn(fun() -> receive after 5000
... end)` placed right after `Bandit.start_link` prints
`probe_start` immediately and then never advances, even though
other schedulers ARE calling `futex(..., timespec*)` with
timeouts (we logged 30+ such calls per Bandit run, with
durations of 44+ seconds). The probe's scheduler may be busy
running Bandit's supervision tasks and never reaching its own
sleep path; or its 5-second timer is in a wheel that doesn't
get advanced because the scheduler never enters `ethr_event_twait`.

### B2.7. ERTS scheduler instrumentation: probe's scheduler is mostly asleep (NEW)

Patched ERTS with raw-byte serial markers in `erl_process.c`:
- 0xD0 every iteration of the main `schedule()` loop
- 0xD1 when `check_time_reds >= ERTS_CHECK_TIME_REDS` (refreshing time)
- 0xD2 when `last_monotonic_time >= next_timeout_time` (wheel ready)
- 0xD3 when `erts_bump_timers` actually fires
- 0xD4 in `enqueue_process` to track which run queue receives work

A second patch dumps the actual `last_monotonic_time` /
`next_timeout_time` values via `erts_fprintf` so we can see what
the comparison looks like.

Run with `Bandit.start_link` + a probe `receive after 5000` for
25 seconds. Per-scheduler counts (sched 1 / sched 2):

| Marker | sched 1 | sched 2 |
|---|---:|---:|
| schedule() loop top    | 788  | 2186 |
| check_time refresh     | 1    | 7    |
| timer-due hit          | 1    | 0    |
| bump_timers fired      | 1    | 0    |

And the `(mt, nt)` value dumps from sched 2:

```
TYN-T sno=2 mt=4592711088 nt=49152000000 …
TYN-T sno=2 mt=5817700811 nt=49152000000 …
TYN-T sno=2 mt=5829699820 nt=49152000000 …
TYN-T sno=2 mt=5841699834 nt=49152000000 …
```

`mt` (monotonic time) is climbing 4.5B → 5.8B ns (so monotonic
time is correctly advancing on this scheduler). `nt`
(`next_timeout_time`) is **stuck at 49,152,000,000 ns** — a
suspiciously round number that looks like ERTS's
"wheel-empty" sentinel (the next timer-wheel slot boundary far
in the future when no real timer is set). So **scheduler 2's
timer wheel is empty** — Bandit's processes never set a timer
that lands on sched 2's wheel.

The probe must therefore live on scheduler 1. But sched 1 only
ran the schedule loop 788 times in 25 s and refreshed time
exactly once. Most of sched 1's time is spent in
`scheduler_wait` (sleeping). The `bump_timers` calls inside
`scheduler_wait` (lines 3539 and 3619 of `erl_process.c`) — paths
we did *not* instrument in this round — are where the probe's
timer would have to fire.

Two follow-on hypotheses:

1. **`scheduler_wait`'s bump_timers paths aren't firing** because
   the wake-from-sleep mechanism doesn't take that branch. ERTS's
   wait loop has multiple branches; the one that calls
   `bump_timers` requires `aux_work` to be set. If aux_work isn't
   set on sched 1 during the probe's 5-second wait, bump_timers
   never runs and the wheel doesn't advance.
2. **Sched 1's `ethr_event_twait` returns from our futex timeout
   correctly**, but the `scheduler_wait` loop computes a *new*
   timeout immediately and re-sleeps without bumping timers. If
   `next_timeout_time` is the probe's 5s deadline, the timeout
   should fire bump_timers — unless the probe's deadline isn't
   actually in sched 1's wheel either, in which case
   `next_timeout_time` is also "infinite" and scheduler_wait
   sleeps with the maximum timeout.

The next experiment: instrument `scheduler_wait`'s loop top, the
two bump_timers calls inside it, and `next_timeout_time` /
`erts_check_next_timeout_time` return values to see exactly which
branch sched 1 takes during the probe's 5-second wait.

### B2.8. The probe's timer is registered, but its scheduler stops checking the wheel (NEW)

Further instrumentation in `erl_hl_timer.c` (`set_proc_timer_common`)
and `erl_process.c` (`scheduler_wait`'s `tse_twait`/`check_io`/`bump_timers`
calls) confirms:

1. **The probe DOES register its 5-second timer.** With probe pid
   `<0.387.0>`:
   ```
   TYN-PT set sno=1 tmo=5000 pos=20182 which=tw pid=<0.387.0>
   ```
   So the probe's timer is on **sched 1**'s timer wheel, at
   `timeout_pos = 20182` (CLKTCKS — `get_timeout_pos` returns
   the wheel-internal unit, ≈ ms; the probe is set to fire at
   ≈ 20.182 s of monotonic time).

2. **Sched 1's wheel IS being advanced**, but only sporadically:
   ```
   TYN-W wait_bump sno=1 ct=4412M tt=4261M
   TYN-W wait_bump sno=1 ct=5423M tt=5421M
   TYN-W wait_bump sno=1 ct=6429M tt=6428M
   TYN-W wait_bump sno=1 ct=7452M tt=7437M
   TYN-W wait_bump sno=1 ct=8470M tt=8460M
   TYN-W wait_bump sno=1 ct=9480M tt=9477M
   TYN-W wait_bump sno=1 ct=10492M tt=10485M
   ```
   `bump_timers` fires every ~1 s of monotonic time from the
   `scheduler_wait` path until ct = 10.5 s. **After that —
   silence.** Sched 1 stops entering `scheduler_wait`.

3. **The schedule()-loop top-of-loop bump path almost never
   fires** — 2 hits in 25 s. That path requires
   `check_time_reds >= ERTS_CHECK_TIME_REDS` (`= CONTEXT_REDS = 4000`
   reductions) to refresh time before checking. Sched 1 does only
   ~5 reductions per loop iteration on average (788 iterations,
   1 refresh), so the threshold rarely fires.

4. **Between ct=10.5s and the probe's deadline of ct≈20.2s,
   neither bump path runs.** The probe's timer sits in the wheel
   uninspected. It never fires before our 25-second test ends.

The issue is therefore **not** that the timer wheel is broken,
the kernel is dropping wakes, or the futex layer is wrong. Those
are all working. The issue is that ERTS's per-scheduler timer
wheel only advances via two paths:

- `scheduler_wait`'s `bump_timers` (when scheduler sleeps)
- `schedule()`'s top-of-loop `bump_timers` (when reductions
  accumulate to a threshold)

After ct≈10.5 s, sched 1 is busy enough to never sleep, but its
processes execute too few reductions per iteration to trip the
top-of-loop check. The wheel freezes mid-flight.

This shape strongly suggests the root cause is in **how often
Erlang processes manage to actually run on sched 1**. Each
schedule-loop iteration in our system runs ~5 reductions on
average — way below the ~4000 ERTS expects. Possible causes:

- Each process is being context-switched out by our preemption
  trampoline before getting through its reductions, so ERTS
  thinks it ran very little
- Some path in our scheduler causes Erlang processes to yield
  unnaturally quickly
- The timer-tick we use for preemption is too aggressive for
  the work each process is doing

### B2.9. Lowered ERTS_CHECK_TIME_REDS to 100 — no change. Diagnosis wrong (NEW)

Tested two ways:

- **10 Hz timer (preemption every 100 ms instead of 10 ms)** alone:
  no improvement. Probe still doesn't fire, curl still times out
  with 0 bytes.
- **`ERTS_CHECK_TIME_REDS = 100` instead of 4000** (top-of-loop
  bump should fire 40× more often if reductions accumulate
  normally): no improvement. Top-of-loop `bump_timers` fires
  only **3 times** in 25 s of Bandit runtime — essentially the
  same as the 2 hits at threshold=4000.

So the issue isn't "preemption fires before reductions
accumulate to 4000." Even at threshold 100, reductions still
aren't accumulating fast enough. The schedule loop iterates
~788 times in 25 s but processes execute close to zero
reductions per iteration. The loop is running but barely any
actual Erlang work is happening.

Two possible deeper causes:

1. **Most schedule iterations don't run any process.** The loop
   picks something (port? aux work? empty run-queue scan?) and
   advances without consuming process reductions. Logging which
   path each iteration takes would clarify.
2. **Process state is wrong** — processes are being marked
   ACTIVE and put in the run queue, but immediately yield with
   0 reductions because some condition (mailbox empty? state
   check?) sends them back to wait without running their body.
   This would also explain why `proc_timeout_common` setting
   `state |= ACTIVE` and queueing the probe might not actually
   wake it.

### Next investigations

1. **Patch the `schedule()` body** to log every iteration's
   actual reduction count delta per swap-out: `cur_reds -
   prev_reds`. If most are 0, processes aren't running on the
   slot. If all are non-zero but small, processes voluntarily
   yield very fast.
2. **Patch `proc_timeout_common`** to log when it fires — does
   the probe's timer callback actually run? If yes, and probe
   still doesn't wake, the bug is post-callback (process state
   manipulation). If no, the bug is in timer-wheel slot
   processing inside `bump_timers`.
3. **Diff against a Linux ERTS run**: run the same Bandit
   eval on a real Linux box, capture the same markers (sched
   loop iterations, refresh count, bump count, reduction
   counts). Comparison points to whatever the qualitative
   difference is.

### B2.10. The wake-pipe spin: `sys_epoll_wait` never yields on success (DIAGNOSED, fix-in-progress)

**Update at end of this session: yield-on-events fix removed; see B2.11.**


Diagnostic: counted how many `epoll_wait` calls returned with events
vs returned with timeout during a 25-second Bandit run.

```
[ew] events=131073 (n_ready=1, first_fd=200, mtime=...)
[ew] events=262145 (n_ready=1, first_fd=200, mtime=...)
```

**262144 epoll_wait calls returned with events. Zero timeouts.**
The "ready" fd was always **fd 200 — ERTS's wake pipe**. Many
gen_server processes constantly write "!" to it; reads happen but
new writes arrive faster than reads consume, so the pipe stayed
non-empty and our level-triggered `has_data` correctly reported it
ready every iteration. The poll thread was in a 10 kHz busy loop:
epoll_wait → return immediately → read → repeat.

The downstream effect: with the poll thread monopolizing its CPU,
the per-scheduler timer wheels on the OTHER schedulers never got
processed. ERTS's `scheduler_wait → bump_timers` path requires the
sleep to actually return on its TIMEOUT (so `current_time >=
timeout_time` is true). With nothing else running, the wheel
froze.

**Fix in `sys_epoll_wait`:** call `yield_current()` after returning
events, even on the success path. The poll thread still spins, but
between iterations it yields to other threads — giving the
schedulers their turn to wake from `tse_twait` and run their wheel
processing.

```rust
if count > 0 {
    crate::sched::yield_current();
    return count;
}
```

Result: with Bandit running, **the diagnostic probe's `receive
after 5000` fires correctly** (3 of 4 runs print
`probe_5s_fired`). The timer wheel is now advancing again.

But Bandit's HTTP curl response **still doesn't come back**: same
old `accept → fcntl(F_SETFL, O_NONBLOCK) → silence` pattern on
fd 502. So the original Bandit-handler stall is independent of
the timer-wheel issue and remains open.

Boot reliability: 31/32 OK on the 30-second KVM trial (one
timeout). Slight variance from the prior 64/64 — possibly because
the extra yield exposes more thread-scheduling flakiness, or
just normal trial variance. Worth re-confirming with a larger
sample.

### B2.11. yield-on-events exposed a cross-CPU TSC clock bug (kept the diagnosis, dropped the fix)

After committing the diagnosis, a wider reliability run hit a NEW
panic from ERTS:

```
OS monotonic time stepped backwards!
Previous time: 1124340689
Current time:  1124327552
[syscall] exit_group(127)
```

That's a 13 µs backwards jump in `clock_gettime(CLOCK_MONOTONIC_RAW)`
between two reads on the same scheduler. ERTS's `check_os_monotonic_time`
asserts `last <= mtime` and aborts the VM.

Root cause: each AP has its own TSC that we sample with `_rdtsc`,
then add a per-CPU offset measured at boot via 3-round trampoline
sync (`measure_tsc_offset`). The offset has tens of µs of jitter
(the median of 3 rounds isn't tight enough). Our `monotonic_ns`
already has a global ratchet using `LAST_TIME_NS.fetch_max(...)`
which should make this impossible — yet the panic fires. Either
the ratchet has a subtle race I missed, or there's a path that
bypasses it. Diagnostic per-CPU `[mono-bug]` log never fires, which
is consistent with cross-CPU thread migration after the
yield-on-events change made migration much more frequent.

The yield-on-events change made this much more likely because it
moved the poll thread off-and-on its CPU once per epoll_wait
return — at ~10 kHz with Bandit's wake-pipe storm, that's plenty
of opportunities for cross-CPU TSC jitter to leak through.

**Decision for this session:** drop yield-on-events. The simple
`gen_tcp` HTTP demo works without it (the wake pipe doesn't get
hammered the way it does with Bandit). Reliability with the rest
of the kernel fixes (futex/epoll timeouts, 100 Hz watchdog,
sched_yield as no-op) is **31/32 = 97%** on the 30-second KVM
trial — same range as before. The cross-CPU TSC monotonicity bug
is now documented but not yet fixed; it'll resurface if/when we
re-attempt Bandit because the wake-pipe spin will return and need
yielding to break.

### B3. Bandit's handler stall (still the original symptom)

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

1. ~~**Fix B1**~~ DONE — watchdog rescue now actually queues.
2. ~~**Honor futex/epoll_wait timeouts**~~ DONE — sys_futex parses
   the timespec, sys_epoll_wait honors `timeout_ms`. Boot still
   100%. `receive after N` works in non-Bandit evals. Bandit
   path still stalls.
3. **Trace timer-wheel processing on the probe's scheduler**:
   the probe gets scheduled to *some* CPU. Log every scheduler's
   timer-wheel advance counter, find which one runs the probe,
   verify whether that scheduler ever enters `ethr_event_twait`
   (and thus our futex timeout path). If it never sleeps, the
   timer wheel must be advanced by the scheduler's main-loop
   tick, not by a wake — and we need to find why that tick
   isn't reaching the wheel.
4. **`scheduler_wait` instrumentation**: patch ERTS's scheduler
   sleep entry/exit (`scheduler_wait` in `erl_process.c`) to
   log entry, computed timeout, and exit reason. With Bandit
   active, look for a scheduler that NEVER enters scheduler_wait
   — that's the one starving the probe.
5. **Audit IPI delivery**: `apic::send_ipi` is fire-and-forget.
   If the target CPU's APIC is in a state where IPIs are
   masked, we don't notice. Logging IPI destination + delivery
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
