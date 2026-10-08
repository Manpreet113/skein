use std::sync::Arc;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::Mutex;

use super::app::{ALL_ROLES, App, OklchParam, Theme};

pub async fn handle_key(key: KeyEvent, app: Arc<Mutex<App>>) -> Option<Theme> {
    let mut state = app.lock().await;

    match key.code {
        KeyCode::Char('q') => {
            state.should_quit = true;
            return None;
        }
        KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
            state.should_quit = true;
            return None;
        }

        KeyCode::Char('h') | KeyCode::Left => {
            state.selected_role = state.selected_role.saturating_sub(1);
        }
        KeyCode::Char('l') | KeyCode::Right => {
            state.selected_role = (state.selected_role + 1).min(ALL_ROLES.len() - 1);
        }

        KeyCode::Char('j') | KeyCode::Char('-') => {
            let delta = nudge_step(&state.oklch_param);
            state.nudge(-delta);
        }
        KeyCode::Char('k') | KeyCode::Char('+') => {
            let delta = nudge_step(&state.oklch_param);
            state.nudge(delta);
        }

        KeyCode::Char('J') => {
            state.oklch_param = OklchParam::Chroma;
            state.nudge(-0.01);
        }
        KeyCode::Char('K') => {
            state.oklch_param = OklchParam::Chroma;
            state.nudge(0.01);
        }

        KeyCode::Char('[') => {
            state.oklch_param = OklchParam::Hue;
            state.nudge(-2.0);
        }
        KeyCode::Char(']') => {
            state.oklch_param = OklchParam::Hue;
            state.nudge(2.0);
        }

        KeyCode::Tab => {
            state.oklch_param = state.oklch_param.cycle();
        }

        KeyCode::Char('r') => {
            state.reset_to_daemon();
        }

        KeyCode::Enter | KeyCode::Char(' ')
            if state.dirty => {
                return Some(state.local_theme.clone());
            }

        _ => {}
    }

    if state.dirty {
        Some(state.local_theme.clone())
    } else {
        None
    }
}

fn nudge_step(param: &OklchParam) -> f32 {
    match param {
        OklchParam::Lightness => 0.01,
        OklchParam::Chroma => 0.005,
        OklchParam::Hue => 1.0,
    }
}
