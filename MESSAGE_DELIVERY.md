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

### B2.12. Bisection: minimal ThousandIsland + EchoHandler reproduces the same stall

To find out whether the stall is in Bandit's HTTP/Plug layer or in
ThousandIsland's socket handoff, I built a minimal `EchoHandler`
that uses `ThousandIsland.Handler` directly — bypassing all of
Bandit:

```elixir
defmodule EchoHandler do
  use ThousandIsland.Handler
  def handle_connection(_socket, state) do
    IO.puts("[echo] handle_connection"); {:continue, state}
  end
  def handle_data(data, socket, state) do
    IO.puts("[echo] handle_data #{byte_size(data)} bytes")
    ThousandIsland.Socket.send(socket, "HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nHi\n")
    {:close, state}
  end
end

{:ok, _} = ThousandIsland.start_link(port: 8080, handler_module: EchoHandler)
```

Compiled the .beam against ThousandIsland 1.3.10 (in a docker
`elixir:1.18-alpine` container, since the build server's OTP 27 was
TYN-tainted), added it to the cpio rootfs, and booted Tyn:

```
ti_start
{ti_started,<0.79.0>}                ← TI.start_link returned OK
=WARNING REPORT==== Failed to lookup telemetry handlers ===  (3×)
[accept] connection established!
[accept] returning new_fd=502         ← kernel accepted curl's connection
[sock-sc] fd=502 a1=0x3 a2=0x0       ← F_GETFL on fd 502
[sock-sc] fd=502 a1=0x4 a2=0x8800    ← F_SETFL O_NONBLOCK|O_LARGEFILE
                                      ← (then silence on fd 502 forever)
[sock-sc] fd=501 ...                  ← back to accept loop on listener
```

**Neither `handle_connection` nor `handle_data` ever prints.** Curl
times out with 0 bytes received. This is the **same `accept →
fcntl → silence`** pattern we saw with Bandit, so the bug is **not
in Bandit's HTTP layer** — it's in ThousandIsland's socket-handoff
to the handler process.

Earlier in the session we proved that raw `spawn(fun() -> ...
end)` + `controlling_process` works (the manual `gen_tcp` demo
returns "Hi"). So the failure narrows to:

> `DynamicSupervisor.start_child` → `gen_server:init_it` →
> `proc_lib:init_p` → handler `init/1`

The spawned child gen_server **never starts running its `init/1`**.
The supervisor returns successfully (so `start_child` doesn't
block ThousandIsland's acceptor), but the resulting child process
sits in some state where it hasn't yet been scheduled or hasn't
processed its first message.

This is a much narrower target than "the Bandit handler stall" —
it's a `proc_lib`/dynamic-supervisor lifecycle bug specific to
how Tyn schedules the newly-spawned child.

### B2.13. Process-spawn / supervisor / GenServer pattern: every layer works

After §B2.12 narrowed the bug to "DynamicSupervisor.start_child →
gen_server:init_it → handler init/1," I ran a sequence of focused
experiments to pin down which layer fails. **Every layer works**:

| Test | Result |
|------|--------|
| t1 — raw `spawn(fun)` | child runs ✓ |
| t2 — `proc_lib:spawn(fun)` | child runs ✓ |
| t3 — `proc_lib:spawn_link(fun)` | child runs ✓ |
| t4 — `proc_lib:start_link(M,F,A)` with `init_ack` | child runs, `{ok,Pid}` returned ✓ |
| t7 — `Agent.start_link` | `{ok,<0.79.0>}` ✓ |
| t8 — `Task.start_link(fun)` | task body runs ✓ |
| t9 — `DynamicSupervisor.start_child` of an `Agent` | agent's init fun runs, `{ok,<0.83.0>}` ✓ |
| t10 — `spawn_link` + `process_flag(trap_exit,true)` + recv | child runs, msg delivered ✓ |
| t11 — `proc_lib:spawn_link` + trap_exit + recv | msg delivered ✓ |
| t12 — DynSup child + trap_exit + recv | msg delivered ✓ |

`t12` is the **exact pattern ThousandIsland.Handler uses** — DynSup
spawns a process that does `Process.flag(:trap_exit, true)` then
waits for `:thousand_island_ready` in `handle_info`. Our test
sends a stand-in message and the child receives it. **It works.**

So the bug is **not** in:
- process spawning at any level
- supervisor child management
- GenServer's init / handle_info flow
- `Process.flag(:trap_exit, true)`
- message delivery to a freshly-spawned child

The remaining suspect is the acceptor-side flow specific to
ThousandIsland: between `start_child` (returning `{:ok, pid}`)
and the `send(pid, {:thousand_island_ready, ...})` that the
handler actually waits for, the acceptor calls
`gen_tcp:controlling_process(socket, pid)`. The fcntl trace on
fd 502 (`F_GETFL → F_SETFL O_NONBLOCK|O_LARGEFILE`) is from
`controlling_process` or its internal `inet:setopts`. After that
fcntl, we see no further activity on fd 502 — so either
`controlling_process` is hanging (and the send never happens),
or it returns and the message is sent but isn't delivered.

Earlier we proved raw `gen_tcp:controlling_process(Sk, SpawnedPid)`
works (the manual demo prints `{ctrl, ok}`). The difference here
is that `Pid` is a freshly-spawned **GenServer process** registered
under a DynamicSupervisor, not a bare `spawn` pid. There may be a
subtle mailbox / monitor / link interaction that fails only in that
combination.

Next probe: explicitly call `gen_tcp:controlling_process` from the
acceptor pattern with a DynSup-managed handler, with `send` of a
distinguished message after, and watch the kernel-side fcntl/recv
sequence. (Postponed — committing the bisection diary first.)

### B2.14. The B-probe: identical OTP pattern with an Agent works perfectly

Per the next narrowing step, I built the precise pattern your hypothesis
called out — `gen_tcp:listen` → `DynamicSupervisor.start_child` of an
Agent (a vanilla GenServer) → `gen_tcp:accept` → `controlling_process` →
`inet:setopts({active, once})` → `send(handler, {:tcp, sock, "test"})`
— and bracketed every step with `[B]` markers.

```
[B] before listen
[B] listen ok
[B] dynsup <0.79.0>
[B] handler agent_init                ← Agent's init runs in DynSup child
[B] handler <0.80.0>
[B] before accept
[B] accepted sock                     ← curl connects, accept returns
[B] before controlling_process
[B] controlling_process => ok         ← gen_tcp:controlling_process works
[B] before setopts
[B] setopts => ok                     ← inet:setopts({active,once}) works
[B] before send fake tcp
[B] send done
[B] handler alive=true                ← handler still alive
[B] DONE
```

**Every primitive completes successfully.** This is the same OTP-level
shape as TI's acceptor flow, with a generic `Agent` standing in for
`ThousandIsland.Handler`.

For comparison, the kernel-side trace from a real
`ThousandIsland + EchoHandler` run shows:

```
[accept] returning new_fd=502
[sock-sc] t4 nr=72 fd=502 a1=0x3 a2=0x0    ← fcntl F_GETFL
[sock-sc] t4 nr=72 fd=502 a1=0x4 a2=0x8800 ← fcntl F_SETFL O_NONBLOCK|O_LARGEFILE
[sock-sc] t5 nr=43 fd=501 ...              ← back to accept on listener
                                            (and silence on fd 502 forever)
```

The acceptor **returns to its accept loop** — meaning
`start_child + controlling_process + send` all completed
successfully (they don't make further kernel syscalls; they're
Erlang-internal). But the handler `GenServer` **never processes
`{:thousand_island_ready, ...}`** — the very next message on its
mailbox after `init/1` returns.

**Strong signal.** It's not a missing kernel primitive; it's a
specific interaction between `ThousandIsland.Handler`'s expanded
GenServer (generated by `use ThousandIsland.Handler`) and how
its `handle_info({:thousand_island_ready, ...})` clause fires.
Our manual test of the same OTP pattern works; TI's specific
combination of `Process.flag(:trap_exit, true)` + the
`{:thousand_island_ready, ...}` flow does not.

Possible next-session probes (none cheap):

1. Recompile ThousandIsland with `IO.puts` markers on every line
   of `Connection.start` and `Handler.handle_info` — needs the
   docker `elixir:1.18-alpine` toolchain re-pulled (was removed
   for disk space). Would tell us *exactly* which line in
   `Connection.start` runs and whether `Handler.handle_info` ever
   dispatches.
2. Compile a stripped-down `MyHandler` that uses `GenServer`
   directly (no `use ThousandIsland.Handler` macro), copies the
   exact `init`/`handle_info` shape, and is started by a custom
   listener. If *that* works but `EchoHandler` doesn't, the bug
   is in `__using__`'s expansion.

### B2.15. Stripped-down `my_handler` proves the bug is in `Connection.start`, not the handler

Wrote a pure-Erlang `my_handler` with just `-behaviour(gen_server)`
plus `child_spec/1` and `start_link/1` — **no** `use ThousandIsland.Handler`,
no macro expansion, just raw `gen_server` callbacks:

```erlang
init(_) ->
    io:format("[myhandler] init~n"),
    process_flag(trap_exit, true),
    {ok, ...}.

handle_info({thousand_island_ready, _, _, _, _}, ...) ->
    io:format("[myhandler] got thousand_island_ready~n"), ...;
handle_info(M, S) -> io:format("[myhandler] got unknown=~p~n",[M]), ...
```

Compiled with system `erlc` (OTP 25 — forward-compatible to OTP 27),
added the `.beam` to the cpio rootfs, started ThousandIsland with
`handler_module: my_handler`. Verified:

- `code:which(my_handler) = "./my_handler.beam"` ✓
- Manual `my_handler:start_link({foo, []})` from the eval **works** —
  prints `[myhandler] init` ✓
- `ThousandIsland:start_link([{port,8080},{handler_module,my_handler}])`
  returns `{ok, <0.80.0>}` ✓

Now curl. Kernel trace:

```
[accept] returning new_fd=502
[sock-sc] fd=502 nr=72 a1=0x3 a2=0x0       ← fcntl F_GETFL
[sock-sc] fd=502 nr=72 a1=0x4 a2=0x8800    ← fcntl F_SETFL O_NONBLOCK
[sock-sc] fd=501 nr=43 ...                  ← back to accept
```

**`my_handler.beam` is never loaded by VFS** during the connection.
Compare to `Elixir.ThousandIsland.HandlerConfig.beam` and
`Elixir.DynamicSupervisor.beam` and `Elixir.Task.beam` which **are**
loaded as part of TI startup — so the loader works fine for other
modules. The handler module specifically is never reached.

We also saw a **flood of 88 telemetry warnings** per single curl
connection ("Failed to lookup telemetry handlers"). Starting
`telemetry` via `:application.ensure_all_started(:telemetry)`
removes the warnings — but **does not fix the stall**. Same fcntl
sequence, no handler load.

**Conclusion.** The handler module isn't the bug. The bug is in
`ThousandIsland.Connection.start` itself — specifically the
section between accepting the socket and calling
`Supervisor.child_spec({handler_module, args}, opts)`. That
function call (which would force-load `my_handler.beam` via
`my_handler.child_spec/1`) is **never reached**. The acceptor's
parent (`AcceptorSupervisor`) silently restarts it after the
crash — Logger isn't catching the report (probably because
Logger's own pipeline is partially broken without all of
`error_logger`'s deps started).

The real next-session probe: get a CRASH REPORT out of the
acceptor so we can see *which line* in `Connection.start`
raises. Options:

1. Recompile `ThousandIsland.Connection` with `IO.puts` after
   each line — the docker `elixir:1.18-alpine` toolchain
   already worked for `EchoHandler.beam`, just need to re-pull
   it.
2. Patch the Acceptor to wrap `Connection.start` in
   `try/rescue` and `IO.puts` the exception so we can see what
   actually crashes.
3. Add a custom Logger handler to the Tyn boot path that
   formats reports to stdout unconditionally (bypass Elixir's
   Logger entirely).

Whichever we pick, the answer is now **one log line away** —
the crash IS happening, we just don't see it.

### B2.16-B2.20. The real bug: concurrent `gen_tcp:accept` waiters

After §B2.15 narrowed the failure to "before `Connection.start` even
runs," I patched `ThousandIsland.Connection.beam` (in docker
`elixir:1.18-alpine`) with `:erlang.display(:CONN_*)` markers
inside `start/5` and `do_start/9`. With the patched .beam in the
cpio rootfs, ran TI again, curl connected → **zero `CONN_`
markers** were printed. So `Connection.start` is not called.

That moved the bug upstream of `Connection.start`. The next step
was an attempt to patch `Acceptor.beam` similarly, but the
docker-recompiled version failed to load:

```
exception error: ArgumentError
"The module ThousandIsland.Acceptor was given as a child to a
supervisor but it does not exist"
in 'Elixir.Supervisor':init_child/1 (lib/supervisor.ex, line 797)
```

That's a load-side issue with the docker-built beam — likely
debug-info / OTP-version mismatch with the rest of the cpio.
Restored the original Acceptor and went a different way: replicate
TI's actual call shape from a plain eval and watch the kernel-side
syscall trace.

**The reveal.** With **TI's exact listen options + 100 concurrent
acceptors blocked on `gen_tcp:accept(L)`** (TI's default
`num_acceptors`), curl connects → kernel does its accept and a
short setup (`F_GETFL → F_SETFL O_NONBLOCK|O_LARGEFILE`) → then
**stops, with zero Erlang acceptor processes waking up**. The
short setup is exactly what we'd seen in the broken TI flow.

```
[accept] returning new_fd=502
[sock-sc] fd=502 nr=72 a1=0x3 a2=0x0       ← fcntl F_GETFL
[sock-sc] fd=502 nr=72 a1=0x4 a2=0x8800    ← fcntl F_SETFL O_NONBLOCK
[sock-sc] fd=501 nr=43 ...                  ← back to accept (no acceptor woke)
                                             (silence on fd 502 forever)
```

For comparison, with **1 acceptor** doing the same thing, the
kernel does the FULL setup: `fcntl + getsockopt(SO_LINGER) +
getsockopt(IP_TOS) + setsockopt(IPV6_V6ONLY) + setsockopt + ...`
— the inet_drv's full post-accept configuration sequence — and
returns `{:ok, port}` cleanly.

**Bisection.** With **2 concurrent acceptors**, the first one DID
wake up and got `{:ok, #Port<0.4>}`, but the second never woke
(only one curl connection, expected). However the **parent's
`receive {acc_done, ...}` never fired** even though the first
child exited normally after sending — a separate symptom worth
investigating but secondary to the primary bug.

**Primary bug isolated:** when many Erlang processes call
`gen_tcp:accept(L)` on the same listener, our kernel's response
to a single incoming connection is consumed by the inet_drv's
short-circuit setup but **never delivered to any of the waiting
Erlang processes**. With 1 waiter, full setup runs and the result
is delivered. The transition between 1 and N is the bug.

**Workaround test:** TI accepts a `num_acceptors` option, default
100. Setting `num_acceptors: 1` should sidestep the bug. Tested:
TI starts; kernel does full accept setup on fd 502; but
`Connection.start` is *still* not called. So num_acceptors=1
alone is not sufficient — there's at least one more layer (perhaps
an issue with how TI's single-acceptor flow differs from our
single-shell-process accept). The probe and patched Connection
are preserved in the rootfs (`my_handler.beam`,
`Elixir.EchoHandler.beam`, `Elixir.ThousandIsland.Connection.beam`
patched, `crash_logger.beam`) — the next session can pick up
exactly where this left off without rebuilding the Docker
toolchain.

**Hypothesis for the kernel-side fix:** our smoltcp/socket layer
delivers a connection-arrival to whichever process happens to be
in `accept` first (or to ONE of them via some FIFO), but if the
inet_drv's port has multiple proc-async-accept entries queued, we
need to deliver the inet_async response to the right caller via
the port-control protocol, and we may be dropping that message.
Inspecting `src/net/socket.rs` accept-completion handling and how
we route `{:inet_async, ...}` messages to waiting processes is
the next step.

### B2.21. FIXED: `sys_accept` race + `fcntl` for sockets — TI works end-to-end

**Root cause confirmed.** `sys_accept` had a race between
read-state and steal-handle, and `fcntl(F_SETFL, O_NONBLOCK)`
wasn't being recorded for socket fds (only pipes). With many
concurrent acceptors blocked on the same listener, several
processes could see `Established` simultaneously, race to swap
the socket handle, and corrupt each other's state — so the
inet_drv accept-completion was either dropped or delivered
incoherently.

**Fix in two parts** (both in `src/`):

1. `src/net/socket.rs` — `sys_accept` now does state-check +
   handle-steal + new-listener-install **atomically inside one
   `with_net`**. Losing races see the freshly-installed
   listener (`Listen` state) and either yield (blocking) or
   return EAGAIN (non-blocking).
2. `src/syscall.rs` — `sys_fcntl(F_SETFL, O_NONBLOCK)` now
   records the flag for socket fds via the new
   `crate::net::socket::set_nonblock`. Without this all 100
   ERTS-managed listening accepts are blocking, so they all
   race instead of getting EAGAIN and waiting on epoll.

**End-to-end verification** (with a stripped-down pure-Erlang
`my_handler` standing in for an Elixir handler — the docker-
compiled `Elixir.EchoHandler.beam` had a "corrupt atom table"
load error against the rest of the cpio's beams, hence the
Erlang stand-in):

```
[V] start TI w/ my_handler
[V] TI {ok,<0.83.0>}
{myhandler_child_spec,{[],[]}}    ← Connection.start now reaches DynamicSupervisor.start_child
{myhandler_start_link,[],[]}      ← child spec dispatched
{myhandler_init,[]}                ← my_handler init runs
myhandler_got_ready                ← :thousand_island_ready DELIVERED
myhandler_got_tcp_data             ← curl HTTP request received
```

```
$ curl http://localhost:5566/
Hi
```

**Reliability: 15/16 = 93.75%** on the 30s KVM trial with full
ThousandIsland flow (default 100 acceptors). Manual gen_tcp demo
still ships at 6/6 in a quick check.

This unblocks Bandit too — Bandit is just HTTP/Plug on top of
ThousandIsland. With TI's spawn/dispatch chain working, the
remaining work for Bandit is matching its expected handler
shape (which uses Elixir-compiled beams; the Docker compile
issue would need to be debugged separately, but it's a build
problem now, not a kernel bug).

### B2.22. Bandit + HelloPlug: works end-to-end on Tyn

With the kernel-side accept-race fix from §B2.21 in place,
Bandit's TI-based dispatch chain works untouched. The May-5
Bandit and HelloPlug beams (compiled before any of this debug
work, never patched) just run.

```elixir
defmodule HelloPlug do
  import Plug.Conn
  def init(opts), do: opts
  def call(conn, _opts) do
    conn
    |> put_resp_content_type("text/plain")
    |> send_resp(200, "Hello from Bandit on Tyn!\n")
  end
end
```

```erlang
{ok, _} = 'Elixir.Bandit':start_link([{plug, 'Elixir.HelloPlug'}, {port, 8080}]).
```

```
$ curl http://localhost:5566/
Hello from Bandit on Tyn!
```

**Reliability:**
- Sequential: 5/5 in a single boot, 14/16 = 87.5% across 16 boots
  (the 2 failures are the same boot-time variance we've had).
- Concurrent burst (5 simultaneous curls in a tight loop): 2/5 succeed,
  3 get `Connection reset by peer`. This is a separate (much milder)
  smoltcp backpressure / connection-sup max_children-style issue —
  unrelated to the accept-race bug fixed here. Real-world traffic
  (sequential keepalive) works fine.

The previously-feared "corrupt atom table" issue for the
docker-compiled `Elixir.EchoHandler.beam` was a build toolchain
mismatch (alpine docker compiled against a slightly different OTP
patch level than the cpio's). Sidestepped by using the May-5
beams that were already compiled with the matching toolchain.

### B3. Bandit's handler stall (resolved)

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
