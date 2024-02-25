use core::{cell::Cell, mem, ops::Bound, sync::atomic::Ordering};

use spinning_top::guard::ArcRwSpinlockWriteGuard;

use crate::{
    context::{arch, contexts, signal::signal_handler, Context},
    interrupt,
    percpu::PercpuBlock,
    ptrace, time, LogicalCpuId,
};

use super::ContextId;

unsafe fn update_runnable(context: &mut Context, cpu_id: LogicalCpuId) -> bool {
    // Ignore already running contexts
    if context.running {
        return false;
    }

    // Ignore contexts stopped by ptrace
    if context.ptrace_stop {
        return false;
    }

    // Ignore contexts assigned to other CPUs
    if !context.sched_affinity.contains(cpu_id) {
        return false;
    }

    //TODO: HACK TO WORKAROUND HANGS BY PINNING TO ONE CPU
    if !context.cpu_id.map_or(true, |x| x == cpu_id) {
        return false;
    }

    // Restore from signal, must only be done from another context to avoid overwriting the stack!
    if context.ksig_restore {
        let was_singlestep = ptrace::regs_for(context)
            .map(|s| s.is_singlestep())
            .unwrap_or(false);

        let ksig = context
            .ksig
            .take()
            .expect("context::switch: ksig not set with ksig_restore");
        context.arch = ksig.0;

        context.kfx.copy_from_slice(&*ksig.1);

        if let Some(ref mut kstack) = context.kstack {
            kstack.copy_from_slice(
                &ksig
                    .2
                    .expect("context::switch: ksig kstack not set with ksig_restore"),
            );
        } else {
            panic!("context::switch: kstack not set with ksig_restore");
        }

        context.ksig_restore = false;

        // Keep singlestep flag across jumps
        if let Some(regs) = ptrace::regs_for_mut(context) {
            regs.set_singlestep(was_singlestep);
        }

        context.unblock_no_ipi();
    }

    // Unblock when there are pending signals
    if context.status.is_soft_blocked() && !context.pending.is_empty() {
        context.unblock_no_ipi();
    }

    // Wake from sleep
    if context.status.is_soft_blocked() && context.wake.is_some() {
        let wake = context.wake.expect("context::switch: wake not set");

        let current = time::monotonic();
        if current >= wake {
            context.wake = None;
            context.unblock_no_ipi();
        }
    }

    // Switch to context if it needs to run
    context.status.is_runnable()
}

struct SwitchResult {
    _prev_guard: ArcRwSpinlockWriteGuard<Context>,
    _next_guard: ArcRwSpinlockWriteGuard<Context>,
}

pub fn tick() {
    let ticks_cell = &PercpuBlock::current().switch_internals.pit_ticks;

    let new_ticks = ticks_cell.get() + 1;
    ticks_cell.set(new_ticks);

    // Switch after 3 ticks (about 6.75 ms)
    if new_ticks >= 3 {
        let _ = unsafe { switch() };
    }
}

pub unsafe extern "C" fn switch_finish_hook() {
    if let Some(switch_result) = PercpuBlock::current().switch_internals.switch_result.take() {
        drop(switch_result);
    } else {
        // TODO: unreachable_unchecked()?
        crate::arch::stop::emergency_reset();
    }
    arch::CONTEXT_SWITCH_LOCK.store(false, Ordering::SeqCst);
}

/// Switch to the next context
///
/// # Safety
///
/// Do not call this while holding locks!
pub unsafe fn switch() -> bool {
    let percpu = PercpuBlock::current();

    //set PIT Interrupt counter to 0, giving each process same amount of PIT ticks
    percpu.switch_internals.pit_ticks.set(0);

    // Set the global lock to avoid the unsafe operations below from causing issues
    // TODO: Better memory orderings?
    while arch::CONTEXT_SWITCH_LOCK
        .compare_exchange_weak(false, true, Ordering::SeqCst, Ordering::Relaxed)
        .is_err()
    {
        interrupt::pause();
    }

    let cpu_id = crate::cpu_id();
    let switch_time = crate::time::monotonic();

    let mut switch_context_opt = None;
    {
        let contexts = contexts();

        // Lock previous context
        let prev_context_lock = contexts
            .current()
            .expect("context::switch: not inside of context");
        let prev_context_guard = prev_context_lock.write_arc();

        let idle_id = percpu.switch_internals.idle_id();
        let mut skip_idle = true;

        // Locate next context
        for (pid, next_context_lock) in contexts
            // Include all contexts with IDs greater than the current...
            .range((Bound::Excluded(prev_context_guard.id), Bound::Unbounded))
            .chain(
                contexts
                    // ... and all contexts with IDs less than the current...
                    .range((Bound::Unbounded, Bound::Excluded(prev_context_guard.id))),
            )
            .chain(
                contexts
                    // ... and finally the idle ID
                    .range((Bound::Included(idle_id), Bound::Included(idle_id))),
            )
        // ... but not the current context, which is already locked
        {
            if pid == &idle_id && skip_idle {
                // Skip idle process the first time it shows up
                skip_idle = false;
                continue;
            }

            // Lock next context
            let mut next_context_guard = next_context_lock.write_arc();

            // Update state of next context and check if runnable
            if update_runnable(&mut *next_context_guard, cpu_id) {
                // Store locks for previous and next context
                switch_context_opt = Some((prev_context_guard, next_context_guard));
                break;
            } else {
                continue;
            }
        }
    };

    // Switch process states, TSS stack pointer, and store new context ID
    if let Some((mut prev_context_guard, mut next_context_guard)) = switch_context_opt {
        // Set old context as not running and update CPU time
        let prev_context = &mut *prev_context_guard;
        prev_context.running = false;
        prev_context.cpu_time += switch_time.saturating_sub(prev_context.switch_time);

        // Set new context as running and set switch time
        let next_context = &mut *next_context_guard;
        next_context.running = true;
        next_context.cpu_id = Some(cpu_id);
        next_context.switch_time = switch_time;

        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            if let Some(ref stack) = next_context.kstack {
                crate::gdt::set_tss_stack(stack.as_ptr() as usize + stack.len());
            }
        }
        let percpu = PercpuBlock::current();
        percpu.switch_internals.context_id.set(next_context.id);

        if next_context.ksig.is_none() {
            //TODO: Allow nested signals
            if let Some(sig) = next_context.pending.pop_front() {
                // Signal was found, run signal handler
                let arch = next_context.arch.clone();
                let kfx = next_context.kfx.clone();
                let kstack = next_context.kstack.clone();
                next_context.ksig = Some((arch, kfx, kstack, sig));
                next_context.arch.signal_stack(signal_handler, sig);
            }
        }

        // FIXME set th switch result in arch::switch_to instead
        let prev_context =
            mem::transmute::<&'_ mut Context, &'_ mut Context>(&mut *prev_context_guard);
        let next_context =
            mem::transmute::<&'_ mut Context, &'_ mut Context>(&mut *next_context_guard);

        percpu
            .switch_internals
            .switch_result
            .set(Some(SwitchResult {
                _prev_guard: prev_context_guard,
                _next_guard: next_context_guard,
            }));

        arch::switch_to(prev_context, next_context);

        // NOTE: After switch_to is called, the return address can even be different from the
        // current return address, meaning that we cannot use local variables here, and that we
        // need to use the `switch_finish_hook` to be able to release the locks.

        true
    } else {
        // No target was found, unset global lock and return
        arch::CONTEXT_SWITCH_LOCK.store(false, Ordering::SeqCst);

        false
    }
}

#[derive(Default)]
pub struct ContextSwitchPercpu {
    switch_result: Cell<Option<SwitchResult>>,
    pit_ticks: Cell<usize>,

    /// Unique ID of the currently running context.
    context_id: Cell<ContextId>,

    // The ID of the idle process
    idle_id: Cell<ContextId>,
}
impl ContextSwitchPercpu {
    pub fn context_id(&self) -> ContextId {
        self.context_id.get()
    }
    pub unsafe fn set_context_id(&self, new: ContextId) {
        self.context_id.set(new)
    }
    pub fn idle_id(&self) -> ContextId {
        self.idle_id.get()
    }
    pub unsafe fn set_idle_id(&self, new: ContextId) {
        self.idle_id.set(new)
    }
}
