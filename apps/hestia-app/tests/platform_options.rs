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
fn add_host_address_stays_above_the_android_keyboard() {
    use slint::{LogicalPosition, LogicalSize};
    use std::time::Duration;

    i_slint_backend_testing::init_no_event_loop();
    let ui = AppWindow::new().unwrap();
    ui.set_is_android(true);
    ui.set_starting(false);
    ui.show().unwrap();
    ui.window().set_size(LogicalSize::new(412.0, 915.0));
    ui.set_add_visible(true);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(20));
    slint::platform::update_timers_and_animations();
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(400));
    slint::platform::update_timers_and_animations();

    let address = ElementHandle::find_by_element_id(&ui, "AppWindow::address")
        .next()
        .expect("visible Add Host address field");
    let field_bottom = || address.absolute_position().y + address.size().height;
    let original_bottom = field_bottom();
    let keyboard_top = 590.0;
    assert!(original_bottom > keyboard_top);

    ui.window().set_virtual_keyboard(
        LogicalPosition::new(0.0, keyboard_top),
        LogicalSize::new(412.0, 915.0 - keyboard_top),
        i_slint_core::InternalToken,
    );
    assert!(field_bottom() < keyboard_top);

    ui.window().set_virtual_keyboard(
        LogicalPosition::default(),
        LogicalSize::default(),
        i_slint_core::InternalToken,
    );
    assert_eq!(field_bottom(), original_bottom);
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
