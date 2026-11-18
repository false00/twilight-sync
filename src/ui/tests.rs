use super::*;
use i_slint_backend_testing::ElementHandle;

fn flush() {
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(20));
    slint::platform::update_timers_and_animations();
}

fn button(ui: &MainWindow, label: &str) -> Result<ElementHandle> {
    let mut matches: Vec<_> = ElementHandle::find_by_accessible_label(ui, label)
        .filter(|element| {
            element.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Button)
        })
        .collect();
    ensure!(
        matches.len() == 1,
        "Expected one accessible {label:?} control, got {}",
        matches.len()
    );
    matches.pop().context("The accessible control disappeared")
}

fn activate(ui: &MainWindow, label: &str) -> Result<()> {
    button(ui, label)?.invoke_accessible_default_action();
    flush();
    Ok(())
}

fn fixture() -> Result<MainWindow> {
    i_slint_backend_testing::init_no_event_loop();
    let ui = MainWindow::new()?;
    ui.window().set_size(slint::LogicalSize::new(1200.0, 900.0));
    ui.show()?;
    ui.set_can_manage(true);
    ui.set_games(rows(
        model::GAMES
            .iter()
            .map(|&game| widgets::GameCard {
                title: model::title(game).into(),
                subtitle: model::subtitle(game).into(),
                installed: false,
                status: "Example".into(),
            })
            .collect(),
    ));
    Ok(ui)
}

#[test]
fn slint_navigation_game_selection_and_build_gates_route_real_component_actions() -> Result<()> {
    let ui = fixture()?;
    let selections = Rc::new(RefCell::new(Vec::new()));
    let selected = selections.clone();
    ui.on_game_selected(move |game| selected.borrow_mut().push(game));
    activate(&ui, "Select Ocarina of Time")?;
    assert_eq!(ui.get_selected_game(), 2);
    assert_eq!(&*selections.borrow(), &[2]);
    let actions = Rc::new(RefCell::new(Vec::new()));
    let recorded = actions.clone();
    ui.on_action(move |action, index| recorded.borrow_mut().push((action, index)));
    activate(&ui, "Game setup")?;
    assert_eq!(ui.get_section(), 1);
    ui.set_source_mode(true);
    ui.set_source_ready(false);
    activate(&ui, "Check source & tools")?;
    assert_eq!(actions.borrow().last(), Some(&(Action::CheckSource, 0)));
    actions.borrow_mut().clear();
    activate(&ui, "3  Build")?;
    assert_eq!(ui.get_setup_step(), 2);
    let scroll = ElementHandle::find_by_element_type_name(&ui, "ScrollView")
        .next()
        .context("Missing main scroll view")?;
    scroll.scroll(0.0, -1800.0);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(300));
    activate(&ui, "Build & install")?;
    assert!(
        actions.borrow().is_empty(),
        "A disabled source control executed an action"
    );
    ui.set_source_ready(true);
    activate(&ui, "Build & install")?;
    assert_eq!(actions.borrow().last(), Some(&(Action::BuildSource, 0)));
    for (label, section) in [
        ("Devices", 2),
        ("Save history", 3),
        ("Textures", 4),
        ("Settings", 5),
    ] {
        activate(&ui, label)?;
        assert_eq!(ui.get_section(), section);
    }
    activate(&ui, "About")?;
    assert!(ui.get_about_open());
    activate(&ui, "Back to your adventure")?;
    assert!(!ui.get_about_open());
    Ok(())
}

#[test]
fn compared_saves_show_verified_details_and_metadata_warnings() -> Result<()> {
    let ui = fixture()?;
    let detail = "Verified example inventory detail";
    let warning = "Unsupported example save metadata";
    ui.set_remote_saves(rows(vec![widgets::SaveCard {
        title: "Other PC".into(),
        extra: detail.into(),
        warning: warning.into(),
        ..Default::default()
    }]));
    flush();
    let scroll = ElementHandle::find_by_element_type_name(&ui, "ScrollView")
        .next()
        .context("Missing main scroll view")?;
    scroll.scroll(0.0, -4000.0);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(300));
    flush();
    assert!(
        ElementHandle::find_by_accessible_label(&ui, warning)
            .next()
            .is_some()
    );
    assert!(
        ElementHandle::find_by_element_type_name(&ui, "Muted").any(|element| element
            .accessible_label()
            .is_some_and(|label| label.contains(detail)))
    );
    Ok(())
}

#[test]
fn settings_cards_expand_with_their_content_instead_of_overlapping() -> Result<()> {
    let ui = fixture()?;
    ui.set_section(5);
    flush();
    let toggle = ElementHandle::find_by_accessible_label(&ui, "Check for app updates")
        .find(|element| {
            element.accessible_role() == Some(i_slint_backend_testing::AccessibleRole::Checkbox)
        })
        .context("Missing update preference")?;
    let next = ElementHandle::find_by_accessible_label(&ui, "Keep the good things coming")
        .next()
        .context("Missing update section")?;
    ensure!(
        next.absolute_position().y >= toggle.absolute_position().y + toggle.size().height,
        "The next settings card overlaps its predecessor"
    );
    Ok(())
}

#[test]
fn source_wizard_reaches_review_without_a_dead_end_for_every_game() -> Result<()> {
    let ui = fixture()?;
    activate(&ui, "Game setup")?;
    ui.set_source_mode(true);
    for game in [1, 2] {
        ui.set_selected_game(game);
        flush();
        activate(&ui, "2  Game files")?;
        let scroll = ElementHandle::find_by_element_type_name(&ui, "ScrollView")
            .next()
            .context("Missing main scroll view")?;
        scroll.scroll(0.0, -4000.0);
        i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(300));
        flush();
        activate(&ui, "Next: review build")?;
        assert_eq!(ui.get_setup_step(), 2);
    }
    ui.set_selected_game(0);
    flush();
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "2  Game files")
            .next()
            .is_none()
    );
    activate(&ui, "2  Build")?;
    assert_eq!(ui.get_setup_step(), 2);
    Ok(())
}

#[test]
fn source_cancellation_remains_available_after_leaving_the_build_step() -> Result<()> {
    let ui = fixture()?;
    ui.set_section(1);
    ui.set_source_mode(true);
    ui.set_source_busy(true);
    ui.set_operation_busy(true);
    ui.set_can_manage(false);
    ui.set_selected_game(2);
    flush();
    let actions = Rc::new(RefCell::new(Vec::new()));
    let recorded = actions.clone();
    ui.on_action(move |action, index| recorded.borrow_mut().push((action, index)));
    let scroll = ElementHandle::find_by_element_type_name(&ui, "ScrollView")
        .next()
        .context("Missing main scroll view")?;
    scroll.scroll(0.0, -4000.0);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(300));
    flush();
    activate(&ui, "Cancel safely")?;
    assert_eq!(&*actions.borrow(), &[(Action::CancelSource, 0)]);
    Ok(())
}

#[test]
fn visible_navigation_controls_fit_handheld_and_desktop_layouts_in_both_themes() -> Result<()> {
    let ui = fixture()?;
    for dark in [false, true] {
        ui.set_dark_mode(dark);
        for width in [720.0_f32, 900.0, 1200.0] {
            ui.window().set_size(slint::LogicalSize::new(width, 720.0));
            flush();
            for label in [
                "Play",
                "Game setup",
                "Devices",
                "Save history",
                "Textures",
                "Settings",
                "About",
            ] {
                let element = button(&ui, label)?;
                let position = element.absolute_position();
                let size = element.size();
                ensure!(
                    position.x >= -1.0 && position.x + size.width <= width + 1.0,
                    "{label} overflows at {width}px: {position:?} {size:?}"
                );
                ensure!(
                    size.height >= 44.0,
                    "{label} is smaller than the required touch target"
                );
            }
        }
    }
    Ok(())
}
