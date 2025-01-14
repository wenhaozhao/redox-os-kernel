use core::cell::Cell;
use core::ops::Bound;
use core::sync::atomic::Ordering;

use alloc::sync::Arc;

use spin::{RwLock, RwLockWriteGuard};

use crate::context::signal::signal_handler;
use crate::context::{arch, contexts, Context, Status, CONTEXT_ID};
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
use crate::gdt;
use crate::interrupt::irq::PIT_TICKS;
use crate::interrupt;
use crate::ptrace;
use crate::time;

unsafe fn update_runnable(context: &mut Context, cpu_id: usize) -> bool {
    // Ignore already running contexts
    if context.running {
        return false;
    }

    // Ignore contexts stopped by ptrace
    if context.ptrace_stop {
        return false;
    }

    // Take ownership if not already owned
    // TODO: Support unclaiming context, while still respecting the CPU affinity.
    if context.cpu_id == None && context.sched_affinity.map_or(true, |id| id == crate::cpu_id()) {
        context.cpu_id = Some(cpu_id);
        // println!("{}: take {} {}", cpu_id, context.id, *context.name.read());
    }

    // Do not update anything else and return not runnable if this is owned by another CPU
    if context.cpu_id != Some(cpu_id) {
        return false;
    }

    // Restore from signal, must only be done from another context to avoid overwriting the stack!
    if context.ksig_restore {
        let was_singlestep = ptrace::regs_for(context).map(|s| s.is_singlestep()).unwrap_or(false);

        let ksig = context.ksig.take().expect("context::switch: ksig not set with ksig_restore");
        context.arch = ksig.0;

        context.kfx.copy_from_slice(&*ksig.1);

        if let Some(ref mut kstack) = context.kstack {
            kstack.copy_from_slice(&ksig.2.expect("context::switch: ksig kstack not set with ksig_restore"));
        } else {
            panic!("context::switch: kstack not set with ksig_restore");
        }

        context.ksig_restore = false;

        // Keep singlestep flag across jumps
        if let Some(regs) = ptrace::regs_for_mut(context) {
            regs.set_singlestep(was_singlestep);
        }

        context.unblock();
    }

    // Unblock when there are pending signals
    if context.status == Status::Blocked && !context.pending.is_empty() {
        context.unblock();
    }

    // Wake from sleep
    if context.status == Status::Blocked && context.wake.is_some() {
        let wake = context.wake.expect("context::switch: wake not set");

        let current = time::monotonic();
        if current >= wake {
            context.wake = None;
            context.unblock();
        }
    }

    // Switch to context if it needs to run
    context.status == Status::Runnable
}

struct SwitchResult {
    prev_lock: Arc<RwLock<Context>>,
    next_lock: Arc<RwLock<Context>>,
}

#[thread_local]
static SWITCH_RESULT: Cell<Option<SwitchResult>> = Cell::new(None);

pub unsafe extern "C" fn switch_finish_hook() {
    if let Some(SwitchResult { prev_lock, next_lock }) = SWITCH_RESULT.take() {
        prev_lock.force_write_unlock();
        next_lock.force_write_unlock();
    } else {
        // TODO: unreachable_unchecked()?
        core::intrinsics::abort();
    }
    arch::CONTEXT_SWITCH_LOCK.store(false, Ordering::SeqCst);
}

/// Switch to the next context
///
/// # Safety
///
/// Do not call this while holding locks!
pub unsafe fn switch() -> bool {
    // TODO: Better memory orderings?
    //set PIT Interrupt counter to 0, giving each process same amount of PIT ticks
    let _ticks = PIT_TICKS.swap(0, Ordering::SeqCst);

    // Set the global lock to avoid the unsafe operations below from causing issues
    while arch::CONTEXT_SWITCH_LOCK.compare_exchange_weak(false, true, Ordering::SeqCst, Ordering::Relaxed).is_err() {
        interrupt::pause();
    }

    let cpu_id = crate::cpu_id();
    let switch_time = crate::time::monotonic();

    let mut switch_context_opt = None;
    {
        let contexts = contexts();

        // Lock previous context
        let prev_context_lock = contexts.current().expect("context::switch: not inside of context");
        let prev_context_guard = prev_context_lock.write();

        // Locate next context
        for (_pid, next_context_lock) in contexts
            // Include all contexts with IDs greater than the current...
            .range(
                (Bound::Excluded(prev_context_guard.id), Bound::Unbounded)
            )
            .chain(contexts
                // ... and all contexts with IDs less than the current...
                .range((Bound::Unbounded, Bound::Excluded(prev_context_guard.id)))
            )
            // ... but not the current context, which is already locked
        {
            // Lock next context
            let mut next_context_guard = next_context_lock.write();

            // Update state of next context and check if runnable
            if update_runnable(&mut *next_context_guard, cpu_id) {
                // Store locks for previous and next context
                switch_context_opt = Some((
                    Arc::clone(prev_context_lock),
                    RwLockWriteGuard::leak(prev_context_guard) as *mut Context,
                    Arc::clone(next_context_lock),
                    RwLockWriteGuard::leak(next_context_guard) as *mut Context,
                ));
                break;
            } else {
                continue;
            }
        }
    };

    // Switch process states, TSS stack pointer, and store new context ID
    if let Some((prev_context_lock, prev_context_ptr, next_context_lock, next_context_ptr)) = switch_context_opt {
        // Set old context as not running and update CPU time
        let prev_context = &mut *prev_context_ptr;
        prev_context.running = false;
        prev_context.cpu_time += switch_time.saturating_sub(prev_context.switch_time);

        // Set new context as running and set switch time
        let next_context = &mut *next_context_ptr;
        next_context.running = true;
        next_context.switch_time = switch_time;

        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        {
            if let Some(ref stack) = next_context.kstack {
                gdt::set_tss_stack(stack.as_ptr() as usize + stack.len());
            }
        }
        CONTEXT_ID.store(next_context.id, Ordering::SeqCst);

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

        SWITCH_RESULT.set(Some(SwitchResult {
            prev_lock: prev_context_lock,
            next_lock: next_context_lock,
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
