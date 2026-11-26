#![cfg(debug_assertions)]

use hestia_app::AppWindow;
use i_slint_backend_testing::ElementHandle;
use slint::ComponentHandle;

fn row(ui: &AppWindow, label: &str, expected: bool) {
    assert_eq!(
        ElementHandle::find_by_accessible_label(ui, label).next().is_some(),
        expected,
        "{label}"
    );
}

#[test]
fn android_pairing_keeps_both_fields_and_submit_above_the_keyboard() {
    use slint::platform::{PointerEventButton, WindowEvent};
    use slint::{LogicalPosition, LogicalSize};
    use std::{cell::Cell, rc::Rc, time::Duration};

    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_is_android(true);
    ui.set_starting(false);
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(412.0, 915.0));
    ui.set_pair_visible(true);
    ui.set_pair_otp_mode(true);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(20));
    slint::platform::update_timers_and_animations();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(400));
    slint::platform::update_timers_and_animations();

    let element = |id| {
        ElementHandle::find_by_element_id(&ui, id)
            .next()
            .unwrap_or_else(|| panic!("Missing element {id}"))
    };
    let pin = element("AppWindow::otp-pin");
    let passphrase = element("AppWindow::otp-passphrase");
    let pair = element("AppWindow::submit-otp");
    let bottom = |field: &ElementHandle| field.absolute_position().y + field.size().height;
    let pair_bottom = bottom(&pair);
    let set_keyboard = |top| {
        ui.window().set_virtual_keyboard(
            LogicalPosition::new(0.0, if top == 915.0 { 0.0 } else { top }),
            LogicalSize::new(412.0, 915.0 - top),
            i_slint_core::InternalToken,
        );
        slint::platform::update_timers_and_animations();
    };
    let type_text = |value: &str| {
        for character in value.chars() {
            ui.window().dispatch_event(WindowEvent::KeyPressed {
                text: character.to_string().into(),
            });
            ui.window().dispatch_event(WindowEvent::KeyReleased {
                text: character.to_string().into(),
            });
        }
    };

    pin.mock_single_click(PointerEventButton::Left);
    set_keyboard(590.0);
    assert!(bottom(&pin) < 590.0);
    type_text("1234");
    assert_eq!(ui.get_otp_pin(), "1234");

    passphrase.mock_single_click(PointerEventButton::Left);
    set_keyboard(500.0);
    assert!(bottom(&passphrase) < 500.0);
    assert!(bottom(&pair) < 500.0);
    type_text("pair word");
    assert_eq!(ui.get_otp_passphrase(), "pair word");

    let submitted = Rc::new(Cell::new(false));
    let result = submitted.clone();
    ui.on_pair_submit_otp(move |pin, passphrase| {
        assert_eq!(pin, "1234");
        assert_eq!(passphrase, "pair word");
        result.set(true);
    });
    pair.mock_single_click(PointerEventButton::Left);
    assert!(submitted.get());

    // A short IME viewport can show the active field or the footer. Scrolling
    // reaches the footer while leaving the keyboard open.
    passphrase.mock_single_click(PointerEventButton::Left);
    set_keyboard(170.0);
    assert!(passphrase.absolute_position().y >= 24.0);
    assert!(bottom(&passphrase) <= 170.0);
    assert!(bottom(&pair) > 170.0);
    ui.window().dispatch_event(WindowEvent::PointerScrolled {
        position: LogicalPosition::new(206.0, 90.0),
        delta_x: 0.0,
        delta_y: -300.0,
    });
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(400));
    slint::platform::update_timers_and_animations();
    assert!(bottom(&pair) <= 170.0);

    set_keyboard(915.0);
    assert_eq!(bottom(&pair), pair_bottom);
}

#[test]
fn android_host_forms_stay_above_the_keyboard() {
    use slint::{LogicalPosition, LogicalSize};
    use std::time::Duration;

    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_is_android(true);
    ui.set_starting(false);
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(412.0, 915.0));

    for (edit_host, id) in [(false, "AppWindow::add-address"), (true, "AppWindow::host-address")] {
        ui.set_add_visible(!edit_host);
        ui.set_host_edit_visible(edit_host);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(20));
        slint::platform::update_timers_and_animations();
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(400));
        slint::platform::update_timers_and_animations();
        let address = ElementHandle::find_by_element_id(&ui, id).next().unwrap();
        let bottom = || address.absolute_position().y + address.size().height;
        assert!(bottom() > 590.0);
        ui.window().set_virtual_keyboard(
            LogicalPosition::new(0.0, 590.0),
            LogicalSize::new(412.0, 325.0),
            i_slint_core::InternalToken,
        );
        assert!(bottom() < 590.0);
        ui.window().set_virtual_keyboard(
            LogicalPosition::default(),
            LogicalSize::default(),
            i_slint_core::InternalToken,
        );
        assert!(bottom() > 590.0);
    }
}

#[test]
fn settings_follow_the_decoder_and_platform() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_starting(false);
    ui.set_screen(3);
    ui.show().unwrap();
    // Slint excludes clipped controls from accessibility queries.
    ui.window().set_size(slint::LogicalSize::new(1180.0, 3200.0));
    row(&ui, "Orientation", false);
    row(&ui, "Direct video", false);
    row(&ui, "Zero-copy video", false);
    row(&ui, "GPU colour conversion", true);
    row(&ui, "Gamescope session", true);

    ui.set_is_android(true);
    row(&ui, "Orientation", true);
    row(&ui, "Direct video", true);
    row(&ui, "Zero-copy video", true);
    row(&ui, "GPU colour conversion", false);
    row(&ui, "Gamescope session", true);

    ui.set_cfg_codec(4); // PyroWave uses a Vulkan decoder with planar output.
    row(&ui, "Direct video", false);
    row(&ui, "Zero-copy video", false);
    row(&ui, "GPU colour conversion", true);

    ui.set_cfg_hardware_decoding(false);
    row(&ui, "GPU colour conversion", false);
    ui.set_is_android(false);
    row(&ui, "GPU colour conversion", true); // Also used by Linux software decode.
}

#[test]
fn stream_menu_distinguishes_touch_from_window_controls() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_starting(false);
    ui.set_screen(4);
    ui.set_stream_menu_open(true);
    ui.show().unwrap();
    for android in [false, true] {
        ui.set_is_android(android);
        ui.set_stream_pointer_capture_available(!android);
        row(&ui, "Full screen", !android);
        row(&ui, "Capture mouse", !android);
        for touch in [false, true] {
            ui.set_has_touch_input(touch);
            row(&ui, "Touch", touch);
            row(&ui, "On-screen controller", touch);
            row(&ui, "Zoom", touch);
            row(&ui, "Picture", true);
        }
    }
}

#[test]
fn remote_skips_absent_clipboard_and_touch_rows() {
    use slint::platform::{Key, WindowEvent};
    use std::{cell::Cell, rc::Rc, time::Duration};

    i_slint_backend_testing::init_no_event_loop();
    for android in [false, true] {
        let ui = AppWindow::new().unwrap();
        ui.set_is_android(android);
        ui.set_stream_pointer_capture_available(!android);
        ui.set_starting(false);
        ui.set_screen(4);
        ui.show().unwrap();
        ui.set_stream_menu_open(true);
        let selected = Rc::new(Cell::new(""));
        let result = selected.clone();
        ui.on_stream_toggle_mouse_capture(move || result.set("mouse"));
        let result = selected.clone();
        ui.on_stream_cycle_scale(move || result.set("picture"));

        // Disconnect, keyboard, then the first available input or picture action.
        for key in [Key::DownArrow, Key::DownArrow, Key::DownArrow, Key::Return] {
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(400));
            slint::platform::update_timers_and_animations();
            ui.window().dispatch_event(WindowEvent::KeyPressed { text: key.into() });
            ui.window()
                .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
        }
        assert_eq!(selected.get(), if android { "picture" } else { "mouse" });
        ui.hide().unwrap();
    }
}
