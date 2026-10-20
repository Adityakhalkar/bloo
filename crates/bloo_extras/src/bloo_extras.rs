//! Bloo-only features, kept in one crate so the fork touches as few upstream files as possible.

mod file_history;
mod peek_definition;
mod system_motion;
mod vercel_status;

pub use file_history::ShowFileHistory;
pub use peek_definition::{
    NextPeekResult, OpenPeekedDefinition, PeekDefinition, PreviousPeekResult,
};
pub use system_motion::system_prefers_reduced_motion;
pub use vercel_status::VercelStatus;

pub fn init(cx: &mut gpui::App) {
    file_history::init(cx);
    peek_definition::init(cx);
    vercel_status::init(cx);
}
