//! Port of `packages/core/src/crossfade`: the transition plan, the equal-power gain curve and the data behind the
//! crossfade debug visualization.

mod compute_crossfade_visualization;
mod compute_transition_plan;
mod equal_power_gain;

pub use compute_crossfade_visualization::{CrossfadeVisualization, CrossfadeVisualizationOptions, TrackVisualization, compute_crossfade_visualization, real_time_for_outgoing_position};
pub use compute_transition_plan::{TransitionPlan, compute_transition_plan};
pub use equal_power_gain::{equal_power_gain, sample_equal_power_gain_curve};
