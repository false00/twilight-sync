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
fn game_file_setup_routes_each_games_inputs_and_clear_actions() -> Result<()> {
    let ui = fixture()?;
    ui.window()
        .set_size(slint::LogicalSize::new(1200.0, 2600.0));
    ui.set_section(1);
    ui.set_selected_game(1);
    ui.set_source_input(r"C:\Games\base.nsp".into());
    ui.set_source_update(r"C:\Games\update.nsp".into());
    ui.set_source_keys(r"C:\Games\prod.keys".into());
    let choices = Rc::new(RefCell::new(Vec::new()));
    let recorded = choices.clone();
    ui.on_choose_input(move |kind| recorded.borrow_mut().push(kind));
    let clears = Rc::new(RefCell::new(Vec::new()));
    let recorded = clears.clone();
    ui.on_clear_input(move |kind| recorded.borrow_mut().push(kind));
    flush();
    for label in [
        "Choose TotK base game (.nsp)",
        "Choose TotK update (.nsp)",
        "Choose prod.keys",
    ] {
        activate(&ui, label)?;
    }
    activate(&ui, "Optional firmware fonts")?;
    activate(&ui, "Choose font folder")?;
    for label in ["Clear game input", "Clear update", "Clear keys selection"] {
        activate(&ui, label)?;
    }
    assert_eq!(&*choices.borrow(), &[0, 1, 2, 3]);
    assert_eq!(&*clears.borrow(), &[0, 1, 2]);
    ui.set_selected_game(2);
    ui.set_source_mode(true);
    flush();
    ui.set_setup_step(1);
    ui.set_source_input(r"C:\Games\Ocarina of Time.nup".into());
    flush();
    assert!(!ui.get_extra_game_inputs());
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "Choose prod.keys")
            .next()
            .is_none()
    );
    activate(&ui, "Choose OOT game (.nup)")?;
    activate(&ui, "Use extracted OOT game files")?;
    activate(&ui, "Choose extracted OOT folder")?;
    activate(&ui, "Clear game input")?;
    assert_eq!(&choices.borrow()[4..], &[0, 4]);
    assert_eq!(&clears.borrow()[3..], &[0]);
    ui.set_can_manage(false);
    flush();
    activate(&ui, "Choose OOT game (.nup)")?;
    assert_eq!(choices.borrow().len(), 6);
    Ok(())
}

#[test]
fn texture_library_routes_install_switch_and_restore_actions() -> Result<()> {
    let ui = fixture()?;
    ui.window()
        .set_size(slint::LogicalSize::new(1200.0, 2600.0));
    ui.set_section(4);
    ui.set_texture_creators(rows(vec![
        widgets::TextureCreator {
            name: "First pack".into(),
            version: "Imported today".into(),
            ..Default::default()
        },
        widgets::TextureCreator {
            name: "Second pack".into(),
            version: "1.2.0".into(),
            has_site: true,
            ..Default::default()
        },
    ]));
    ui.set_packs(rows(vec![
        widgets::PackCard {
            title: "Current pack".into(),
            active: true,
            ..Default::default()
        },
        widgets::PackCard {
            title: "Previous pack".into(),
            ..Default::default()
        },
    ]));
    ui.set_can_import_textures(true);
    ui.set_can_change_textures(true);
    ui.set_textures_active(true);
    let actions = Rc::new(RefCell::new(Vec::new()));
    let recorded = actions.clone();
    ui.on_action(move |action, index| recorded.borrow_mut().push((action, index)));
    let toggles = Rc::new(RefCell::new(Vec::new()));
    let recorded = toggles.clone();
    ui.on_toggle(move |toggle, value| recorded.borrow_mut().push((toggle, value)));
    flush();
    activate(&ui, "Activate")?;
    activate(&ui, "Disable pack and restore original textures")?;
    activate(&ui, "Install ZIP")?;
    assert_eq!(
        &*actions.borrow(),
        &[(Action::ActivateTexture, 1), (Action::DisableTextures, 0)]
    );
    activate(&ui, "First pack · Change")?;
    activate(&ui, "Second pack")?;
    assert_eq!(ui.get_texture_creator(), 1);
    assert_eq!(ui.get_texture_version(), "1.2.0");
    assert!(!ui.get_texture_picker_open());
    assert!(toggles.borrow().contains(&(Toggle::TextureConsent, false)));
    ui.set_texture_consent(true);
    flush();
    activate(&ui, "Visit creator download page")?;
    activate(&ui, "Install ZIP")?;
    activate(&ui, "Install folder")?;
    assert_eq!(
        &actions.borrow()[2..],
        &[
            (Action::OpenTextureSite, 1),
            (Action::ImportTextureZip, 0),
            (Action::ImportTextureFolder, 0),
        ]
    );
    ui.set_texture_recovery(true);
    ui.set_texture_busy(true);
    ui.set_texture_cancellable(true);
    flush();
    activate(&ui, "Cancel installation safely")?;
    assert_eq!(actions.borrow().last(), Some(&(Action::CancelTextures, 0)));
    Ok(())
}

#[test]
fn themed_confirmation_blocks_background_actions_and_delivers_the_choice() -> Result<()> {
    let ui = fixture()?;
    let answers = Rc::new(RefCell::new(Vec::new()));
    let recorded = answers.clone();
    let weak = ui.as_weak();
    ui.on_confirmation_response(move |answer| {
        recorded.borrow_mut().push(answer);
        if let Some(ui) = weak.upgrade() {
            ui.set_confirmation_open(false);
        }
    });
    ui.set_confirmation_title("Update Twilight Sync".into());
    ui.set_confirmation_message("Twilight Sync will restart.".into());
    ui.set_confirmation_open(true);
    flush();
    activate(&ui, "About")?;
    assert!(!ui.get_about_open());
    activate(&ui, "Cancel")?;
    assert!(!ui.get_confirmation_open());
    ui.set_confirmation_open(true);
    flush();
    activate(&ui, "Continue")?;
    assert_eq!(&*answers.borrow(), &[false, true]);
    Ok(())
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
    activate(&ui, "Devices")?;
    assert_eq!(ui.get_section(), 5);
    activate(&ui, "Done")?;
    assert!(!ui.get_about_open());
    Ok(())
}

#[test]
fn paired_game_file_controls_are_available_in_both_setup_methods() -> Result<()> {
    let ui = fixture()?;
    ui.window()
        .set_size(slint::LogicalSize::new(1200.0, 3000.0));
    ui.set_section(1);
    ui.set_can_offer_game_file(true);
    ui.set_can_offer_update(true);
    ui.set_can_offer_keys(true);
    let actions = Rc::new(RefCell::new(Vec::new()));
    let recorded = actions.clone();
    ui.on_action(move |action, index| recorded.borrow_mut().push((action, index)));
    for (game, source) in [(2, true), (1, false), (1, true)] {
        ui.set_selected_game(game);
        ui.set_source_mode(source);
        flush();
        if game == 2 {
            activate(&ui, "Offer OOT .nup")?;
            assert_eq!(actions.borrow().last(), Some(&(Action::OfferGameFile, 0)));
        } else {
            for (label, index) in [
                ("Offer TotK base .nsp", 0),
                ("Offer TotK update .nsp", 1),
                ("Offer prod.keys", 2),
            ] {
                activate(&ui, label)?;
                assert_eq!(
                    actions.borrow().last(),
                    Some(&(Action::OfferGameFile, index))
                );
            }
        }
    }
    ui.set_pending_iso(true);
    ui.set_can_accept_iso(true);
    for section in 0..6 {
        ui.set_section(section);
        flush();
        activate(&ui, "Choose destination & receive")?;
        assert_eq!(actions.borrow().last(), Some(&(Action::AcceptIso, 0)));
        activate(&ui, "Decline file")?;
        assert_eq!(actions.borrow().last(), Some(&(Action::DeclineIso, 0)));
    }
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
    activate(&ui, "Save details")?;
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
    let toggle =
        ElementHandle::find_by_accessible_label(&ui, "Automatically check for app updates")
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

#[test]
fn comparison_requires_an_explicit_verified_branch_and_resets_stale_choices() -> Result<()> {
    let ui = fixture()?;
    ui.set_has_conflict(true);
    ui.set_local_revision("a".repeat(64).into());
    ui.set_remote_revision("b".repeat(64).into());
    let actions = Rc::new(RefCell::new(Vec::new()));
    let recorded = actions.clone();
    ui.on_action(move |action, index| recorded.borrow_mut().push((action, index)));
    activate(&ui, "Compare saves")?;
    assert!(ui.get_comparison_open());
    assert_eq!(ui.get_branch_choice(), -1);
    activate(&ui, "Game setup")?;
    assert_eq!(ui.get_section(), 0, "A modal allowed background navigation");
    activate(&ui, "Select other PC's branch")?;
    activate(&ui, "Choose a branch above")?;
    assert_eq!(&*actions.borrow(), &[(Action::Compare, 0)]);
    ui.set_can_resolve(true);
    activate(&ui, "Select other PC's branch")?;
    assert_eq!(ui.get_branch_choice(), 1);
    activate(&ui, "Use other PC's save")?;
    assert_eq!(actions.borrow().last(), Some(&(Action::UseRemote, 0)));
    ui.set_remote_revision("c".repeat(64).into());
    flush();
    assert_eq!(ui.get_branch_choice(), -1);
    activate(&ui, "Choose a branch above")?;
    assert_eq!(actions.borrow().len(), 2);
    activate(&ui, "Not now")?;
    assert!(!ui.get_comparison_open());
    Ok(())
}

#[test]
fn restored_appearance_reconnect_and_cross_game_decisions_are_actionable() -> Result<()> {
    let ui = fixture()?;
    let appearances = Rc::new(RefCell::new(Vec::new()));
    let recorded = appearances.clone();
    ui.on_appearance_selected(move |mode| recorded.borrow_mut().push(mode));
    ui.set_section(5);
    flush();
    for label in ["System", "Light", "Dark"] {
        activate(&ui, label)?;
    }
    assert_eq!(
        &*appearances.borrow(),
        &[
            widgets::AppearanceMode::System,
            widgets::AppearanceMode::Light,
            widgets::AppearanceMode::Dark
        ]
    );
    let actions = Rc::new(RefCell::new(Vec::new()));
    let recorded = actions.clone();
    ui.on_action(move |action, index| recorded.borrow_mut().push((action, index)));
    ui.set_attention(rows(vec![widgets::AttentionCard {
        game_index: 2,
        title: "Ocarina of Time".into(),
        detail: "Incoming save".into(),
    }]));
    flush();
    activate(&ui, "Review decision")?;
    assert_eq!(actions.borrow().last(), Some(&(Action::ViewGame, 2)));
    ui.set_attention(rows(Vec::new()));
    ui.set_section(2);
    ui.set_peer_key("b".repeat(64).into());
    ui.set_can_reconnect(false);
    flush();
    let count = actions.borrow().len();
    activate(&ui, "Reconnect")?;
    assert_eq!(actions.borrow().len(), count);
    ui.set_can_reconnect(true);
    activate(&ui, "Reconnect")?;
    assert_eq!(actions.borrow().last(), Some(&(Action::Reconnect, 0)));
    activate(&ui, "Verified device identity")?;
    assert!(
        ElementHandle::find_by_accessible_label(&ui, &"b".repeat(64))
            .next()
            .is_some()
    );
    Ok(())
}

#[test]
fn active_pairing_prioritizes_the_code_and_both_approvals_in_a_short_window() -> Result<()> {
    let ui = fixture()?;
    ui.window().set_size(slint::LogicalSize::new(720.0, 600.0));
    ui.set_section(2);
    ui.set_pairing_code("1234 5678".into());
    ui.set_remote_approved(true);
    flush();
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "1234 5678")
            .next()
            .is_some()
    );
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "This PC · waiting")
            .next()
            .is_some()
    );
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "Other PC · approved")
            .next()
            .is_some()
    );
    assert!(
        ElementHandle::find_by_accessible_label(&ui, "Open pairing")
            .next()
            .is_none()
    );
    let approve = button(&ui, "Codes match")?;
    ensure!(
        approve.absolute_position().y + approve.size().height <= 600.0,
        "Pairing approval requires scrolling past discovery instead of focusing the code"
    );
    Ok(())
}

#[test]
fn history_preview_preserves_save_dates_details_and_validation_warnings() -> Result<()> {
    let ui = fixture()?;
    ui.set_section(3);
    ui.set_history(rows(vec![widgets::HistoryCard {
        id: "snapshot-id".into(),
        title: "Preserved save".into(),
        metadata: "Parent: verified-parent\nPartner branch: verified-peer-parent".into(),
        ..Default::default()
    }]));
    ui.set_preview_id("snapshot-id".into());
    ui.set_preview_title("Read-only progress".into());
    ui.set_preview_saves(rows(vec![widgets::SaveCard {
        title: "Preview slot".into(),
        date: "2026-10-25 14:15 UTC".into(),
        extra: "Horse: Epona".into(),
        warning: "Unsupported optional metadata; no invented progress".into(),
        ..Default::default()
    }]));
    flush();
    let scroll = ElementHandle::find_by_element_type_name(&ui, "ScrollView")
        .next()
        .context("Missing main scroll view")?;
    scroll.scroll(0.0, -4000.0);
    i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(300));
    flush();
    for label in [
        "2026-10-25 14:15 UTC",
        "Horse: Epona",
        "Unsupported optional metadata; no invented progress",
    ] {
        ensure!(
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .is_some(),
            "History preview lost {label}"
        );
    }
    Ok(())
}

#[test]
fn feature_pages_and_comparison_keep_touch_controls_inside_handheld_bounds() -> Result<()> {
    let ui = fixture()?;
    ui.set_source_mode(true);
    ui.set_selected_game(2);
    ui.set_advanced_source(true);
    ui.set_pending_save(true);
    ui.set_has_conflict(true);
    ui.set_comparison_reason(
        "Wait for both verified snapshots and finish this game's active session.".into(),
    );
    let save = widgets::SaveCard {
        title: "Slot 1 · Example player with a long name".into(),
        hearts: "Unavailable".into(),
        rupees: "Unavailable".into(),
        played: "Unavailable".into(),
        ..Default::default()
    };
    ui.set_local_saves(rows(vec![save.clone()]));
    ui.set_remote_saves(rows(vec![save]));
    ui.set_texture_creators(rows(vec![widgets::TextureCreator {
        name: "Authorized custom texture import".into(),
        description: "Synthetic creator description".into(),
        ..Default::default()
    }]));
    ui.set_history(rows(vec![widgets::HistoryCard {
        title: "Snapshot #12".into(),
        ..Default::default()
    }]));
    for dark in [false, true] {
        ui.set_dark_mode(dark);
        for width in [720.0_f32, 900.0, 1200.0] {
            ui.window().set_size(slint::LogicalSize::new(width, 720.0));
            for section in 0..6 {
                ui.set_section(section);
                for step in if section == 1 { 0..3 } else { 0..1 } {
                    ui.set_setup_step(step);
                    flush();
                    for element in ElementHandle::find_by_element_type_name(&ui, "ActionButton") {
                        let position = element.absolute_position();
                        let size = element.size();
                        ensure!(
                            position.x >= -1.0 && position.x + size.width <= width + 1.0,
                            "{:?} overflows section {section}, step {step}, {width}px: {position:?} {size:?}",
                            element.accessible_label()
                        );
                        ensure!(
                            size.height >= 44.0,
                            "A feature control lost its touch target"
                        );
                    }
                }
            }
            ui.set_section(0);
            ui.set_has_conflict(true);
            ui.set_comparison_open(true);
            flush();
            for label in ["Choose a branch above", "Not now"] {
                let element = button(&ui, label)?;
                let position = element.absolute_position();
                let size = element.size();
                ensure!(
                    position.x >= 0.0
                        && position.x + size.width <= width
                        && position.y >= 0.0
                        && position.y + size.height <= 720.0,
                    "Comparison footer left the viewport at {width}px: {label}"
                );
            }
            ui.set_comparison_open(false);
        }
    }
    Ok(())
}
