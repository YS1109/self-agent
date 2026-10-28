//! Acknowledgements describe transport application, not model compliance.
#[derive(Clone, Debug)]
pub(crate) enum InputAcknowledgement {
    Applied { via: String },
    NotApplied { reason: String },
}
