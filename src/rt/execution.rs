use crate::rt::{object, thread, Access, Path};
use crate::rt::alloc::Allocation;

use std::collections::HashMap;
use std::fmt;

pub(crate) struct Execution {
    /// Uniquely identifies an execution
    pub(super) id: Id,

    /// Execution path taken
    pub(crate) path: Path,

    pub(crate) threads: thread::Set,

    /// All loom aware objects part of this execution run.
    pub(super) objects: object::Store,

    /// Maps raw allocations to LeakTrack objects
    pub(super) raw_allocations: HashMap<usize, Allocation>,

    /// Maximum number of concurrent threads
    pub(super) max_threads: usize,

    pub(super) max_history: usize,

    /// Log execution output to STDOUT
    pub(crate) log: bool,
}

#[derive(Debug, Eq, PartialEq, Hash, Clone, Copy)]
pub(crate) struct Id(usize);

impl Execution {
    /// Create a new execution.
    ///
    /// This is only called at the start of a fuzz run. The same instance is
    /// reused across permutations.
    pub(crate) fn new(
        max_threads: usize,
        max_branches: usize,
        preemption_bound: Option<usize>,
    ) -> Execution {
        let id = Id::new();
        let threads = thread::Set::new(id, max_threads);

        Execution {
            id,
            path: Path::new(max_branches, preemption_bound),
            threads,
            objects: object::Store::new(id),
            raw_allocations: HashMap::new(),
            max_threads,
            max_history: 7,
            log: false,
        }
    }

    /// Create state to track a new thread
    pub(crate) fn new_thread(&mut self) -> thread::Id {
        let thread_id = self.threads.new_thread();
        let active_id = self.threads.active_id();

        let (active, new) = self.threads.active2_mut(thread_id);

        new.causality.join(&active.causality);
        new.dpor_vv.join(&active.dpor_vv);

        // Bump causality in order to ensure CausalCell accurately detects
        // incorrect access when first action.
        new.causality[thread_id] += 1;
        active.causality[active_id] += 1;

        thread_id
    }

    /// Resets the execution state for the next execution run
    pub(crate) fn step(self) -> Option<Self> {
        let id = Id::new();
        let max_threads = self.max_threads;
        let max_history = self.max_history;
        let log = self.log;
        let mut path = self.path;
        let mut objects = self.objects;
        let mut raw_allocations = self.raw_allocations;

        let mut threads = self.threads;

        objects.clear();
        raw_allocations.clear();

        if !path.step() {
            return None;
        }

        threads.clear(id);

        Some(Execution {
            id,
            path,
            threads,
            objects,
            raw_allocations,
            max_threads,
            max_history,
            log,
        })
    }

    /// Returns `true` if a switch is required
    pub(crate) fn schedule(&mut self) -> bool {
        use crate::rt::path::Thread;

        // Implementation of the DPOR algorithm.

        let curr_thread = self.threads.active_id();

        for (th_id, th) in self.threads.iter() {
            let operation = match th.operation {
                Some(operation) => operation,
                None => continue,
            };

            for access in self.objects.last_dependent_accesses(operation) {
                if access.happens_before(&th.dpor_vv) {
                    // The previous access happened before this access, thus
                    // there is no race.
                    continue;
                }

                self.path.backtrack(access.path_id(), th_id);
            }
        }

        // It's important to avoid pre-emption as much as possible
        let mut initial = Some(self.threads.active_id());

        // If the thread is not runnable, then we can pick any arbitrary other
        // runnable thread.
        if !self.threads.active().is_runnable() {
            initial = None;

            for (i, th) in self.threads.iter() {
                if !th.is_runnable() {
                    continue;
                }

                if let Some(ref mut init) = initial {
                    if th.yield_count < self.threads[*init].yield_count {
                        *init = i;
                    }
                } else {
                    initial = Some(i)
                }
            }
        }

        let path_id = self.path.pos();

        let next = self.path.branch_thread(self.id, {
            self.threads.iter().map(|(i, th)| {
                if initial.is_none() && th.is_runnable() {
                    initial = Some(i);
                }

                if initial == Some(i) {
                    Thread::Active
                } else if th.is_yield() {
                    Thread::Yield
                } else if !th.is_runnable() {
                    Thread::Disabled
                } else {
                    Thread::Skip
                }
            })
        });

        let switched = Some(self.threads.active_id()) != next;

        self.threads.set_active(next);

        // There is no active thread. Unless all threads have terminated, the
        // test has deadlocked.
        if !self.threads.is_active() {
            let terminal = self.threads.iter().all(|(_, th)| th.is_terminated());

            assert!(
                terminal,
                "deadlock; threads = {:?}",
                self.threads
                    .iter()
                    .map(|(i, th)| { (i, th.state) })
                    .collect::<Vec<_>>()
            );

            return true;
        }

        // TODO: refactor
        if let Some(operation) = self.threads.active().operation {
            let threads = &mut self.threads;
            let th_id = threads.active_id();

            for access in self.objects.last_dependent_accesses(operation) {
                threads.active_mut().dpor_vv.join(access.version());
            }

            threads.active_mut().dpor_vv[th_id] += 1;

            self.objects
                .set_last_access(operation, Access::new(path_id, &threads.active().dpor_vv));
        }

        // Reactivate yielded threads, but only if the current active thread is
        // not yielded.
        for (id, th) in self.threads.iter_mut() {
            if th.is_yield() && Some(id) != next {
                th.set_runnable();
            }
        }

        if self.log && switched {
            println!("~~~~~~~~ THREAD {} ~~~~~~~~", self.threads.active_id());
        }

        curr_thread != self.threads.active_id()
    }

    /// Panics if any leaks were detected
    pub(crate) fn check_for_leaks(&self) {
        self.objects.check_for_leaks();
    }

    pub(crate) fn set_critical(&mut self) {
        self.threads.active_mut().critical = true;
    }

    pub(crate) fn unset_critical(&mut self) {
        self.threads.active_mut().critical = false;
    }
}

impl fmt::Debug for Execution {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt.debug_struct("Execution")
            .field("path", &self.path)
            .field("threads", &self.threads)
            .finish()
    }
}

impl Id {
    pub(crate) fn new() -> Id {
        use std::sync::atomic::AtomicUsize;
        use std::sync::atomic::Ordering::Relaxed;

        static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

        let next = NEXT_ID.fetch_add(1, Relaxed);

        Id(next)
    }
}
