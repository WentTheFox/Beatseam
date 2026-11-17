//! Port of `packages/core/src/tasks`: the queue that makes long-running library work (scans, metadata and lyrics
//! passes) take turns instead of racing each other for the same file APIs. See [`TaskQueue`].

mod abort;
mod gate;
mod queue;
#[cfg(test)]
mod test_executor;

pub use abort::{AbortHandle, AbortSignal, Aborted, abortable};
pub use gate::Gate;
pub use queue::{PassRegistrar, Subscription, TaskContext, TaskInfo, TaskOptions, TaskPriority, TaskProgress, TaskQueue, TaskState};
