#![cfg(debug_assertions)]

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use hestia_app::AppWindow;
use i_slint_backend_testing::ElementHandle;
use slint::ComponentHandle;
use slint::platform::{Key, WindowEvent};

#[test]
fn launch_can_be_cancelled_and_its_artwork_leaves_the_stream() {
    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_starting(false);
    ui.show().unwrap();
    let cancelled = Rc::new(Cell::new(false));
    let signal = cancelled.clone();
    ui.on_back_to_apps(move || signal.set(true));

    let has_art = || {
        ElementHandle::find_by_accessible_label(&ui, "Cancel launch")
            .next()
            .is_some()
    };
    ui.set_screen(7);
    slint::platform::update_timers_and_animations();
    assert!(has_art());
    ui.window().dispatch_event(WindowEvent::KeyPressed {
        text: Key::Escape.into(),
    });
    assert!(cancelled.get());

    // A failed launch returns directly to its details page.
    ui.set_screen(2);
    slint::platform::update_timers_and_animations();
    assert!(!has_art());

    ui.set_screen(7);
    slint::platform::update_timers_and_animations();
    assert!(has_art());
    ui.set_screen(4);
    slint::platform::update_timers_and_animations();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(200));
    slint::platform::update_timers_and_animations();
    assert!(!has_art());
}
