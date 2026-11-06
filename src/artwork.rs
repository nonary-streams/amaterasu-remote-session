//! Original compact symbolic artwork owned by this plugin.
use crate::identity::Control;

pub(crate) fn png(control: Control) -> &'static [u8] {
    match control {
        Control::Resume => include_bytes!("assets/resume.png"),
        Control::Monitor => include_bytes!("assets/monitor.png"),
        Control::Input => include_bytes!("assets/input.png"),
        Control::Terminate => include_bytes!("assets/terminate.png"),
        Control::DisconnectMonitor => include_bytes!("assets/disconnect-monitor.png"),
        Control::DisconnectInput => include_bytes!("assets/disconnect-input.png"),
        Control::RunningGame => &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    #[test]
    fn controls_have_distinct_non_placeholder_bounded_pngs() {
        let mut images = BTreeSet::new();
        for control in [
            Control::Resume,
            Control::Monitor,
            Control::Input,
            Control::Terminate,
            Control::DisconnectMonitor,
            Control::DisconnectInput,
        ] {
            let bytes = png(control);
            assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
            assert_eq!(&bytes[16..24], &[0, 0, 0, 64, 0, 0, 0, 64]);
            assert!(bytes.len() <= 4096);
            assert!(images.insert(bytes));
        }
    }
}
