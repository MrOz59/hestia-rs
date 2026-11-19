# Hestia RS changes to Slint's Android backend (v1.19.0)

1. **Input hook** (`lib.rs`: `set_input_hook`; `androidwindowadapter.rs`:
   `process_inputs`). The application sees every key and motion event
   before Slint and can consume it. Slint treats joystick motion as touch
   and maps gamepad buttons to nothing, so without this a native-activity
   app can't read game controllers at all.
2. **Mouse wheel** (`androidwindowadapter.rs`: `MotionAction::Scroll`).
   Upstream has `todo!()` there, so any wheel or trackpad scroll from a
   mouse (Samsung DeX, Bluetooth mice) panicked the app. It now dispatches
   `WindowEvent::PointerScrolled`.

Everything else is unchanged. To update: copy the new upstream directory,
reapply these two changes, and bump the pinned slint version in the root
Cargo.toml together with `version` here.
