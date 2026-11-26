use super::Accounts;
use crate::claude_accounts::profile_state::{ProfileState, Tone};

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn signed_out(state: &ProfileState) -> bool {
    state
        .login
        .as_ref()
        .is_some_and(|(_, tone)| *tone == Tone::Bad)
}

pub(super) fn target(
    names: &[String],
    states: &[ProfileState],
    saved: &str,
    after: Option<&str>,
) -> Option<usize> {
    let position = |name: &str| names.iter().position(|candidate| candidate == name);
    let saved_index = position(saved);
    let group = |index: usize| {
        states
            .get(index)
            .and_then(|state| state.same_as.as_deref())
            .and_then(position)
            .unwrap_or(index)
    };
    let candidate = |index: usize| {
        Some(index) != saved_index
            && saved_index.is_none_or(|saved| group(saved) != group(index))
            && !states.get(index).is_some_and(signed_out)
    };
    if after.is_none()
        && let Some(index) = states.iter().position(|state| state.suggested)
        && candidate(index)
    {
        return Some(index);
    }
    let start = after.and_then(position).or(saved_index).unwrap_or(0);
    (1..=names.len())
        .map(|step| (start + step) % names.len())
        .find(|index| candidate(*index) && Some(names[*index].as_str()) != after)
}

pub(super) fn detail(state: Option<&ProfileState>) -> Option<String> {
    let state = state?;
    let window = |label: &str, view: &Option<crate::claude_accounts::profile_state::WindowView>| {
        view.as_ref()
            .map(|view| format!("{label} {}%", view.percent))
    };
    let parts: Vec<String> = [
        state.account.clone(),
        window("5h", &state.five_hour),
        window("week", &state.seven_day),
    ]
    .into_iter()
    .flatten()
    .collect();
    (!parts.is_empty()).then(|| parts.join(" · "))
}

impl Accounts {
    pub(in crate::tui) fn quick_switch(&mut self) {
        if !self.local || self.controls.active() {
            return;
        }
        self.controls.clear_message();
        if self.overridden {
            self.pending = None;
            return;
        }
        let after = self.pending.take().map(|(name, _)| name);
        self.confirmation_visible = false;
        let states = self
            .details
            .roster(&self.names, &self.identities)
            .states(&self.limits, now());
        let index = target(&self.names, &states, &self.selected, after.as_deref()).or_else(|| {
            let after = after.as_deref()?;
            self.names.iter().position(|name| name == after)
        });
        match index {
            Some(index) => {
                self.focus = index;
                self.status.clear();
                self.pending = Some((self.names[index].clone(), self.selected.clone()));
            }
            None => self.status = "No other profile to switch to".into(),
        }
    }

    pub(super) fn pending_detail(&self) -> Option<String> {
        let (name, _) = self.pending.as_ref()?;
        let index = self.names.iter().position(|candidate| candidate == name)?;
        let states = self
            .details
            .roster(&self.names, &self.identities)
            .states(&self.limits, now());
        detail(states.get(index))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        ["default", "backup", "old", "team", "work"]
            .map(String::from)
            .to_vec()
    }

    fn states() -> Vec<ProfileState> {
        let mut states = vec![ProfileState::default(); 5];
        states[2].login = Some(("not signed in".into(), Tone::Bad));
        states[3].same_as = Some("default".into());
        states[4].suggested = true;
        states
    }

    #[test]
    fn first_press_takes_the_suggested_profile() {
        assert_eq!(target(&names(), &states(), "default", None), Some(4));
    }

    #[test]
    fn without_a_suggestion_the_next_profile_in_order_wins() {
        let mut states = states();
        states[4].suggested = false;
        assert_eq!(target(&names(), &states, "default", None), Some(1));
        assert_eq!(target(&names(), &states, "work", None), Some(0));
    }

    #[test]
    fn repeat_press_cycles_and_skips_saved_same_account_and_signed_out() {
        let (names, states) = (names(), states());
        assert_eq!(target(&names, &states, "default", Some("work")), Some(1));
        assert_eq!(target(&names, &states, "default", Some("backup")), Some(4));
    }

    #[test]
    fn a_lone_candidate_does_not_cycle_to_itself_and_no_candidate_is_none() {
        let names: Vec<String> = ["default", "team"].map(String::from).to_vec();
        let mut states = vec![ProfileState::default(); 2];
        assert_eq!(target(&names, &states, "default", None), Some(1));
        assert_eq!(target(&names, &states, "default", Some("team")), None);
        states[1].same_as = Some("default".into());
        assert_eq!(target(&names, &states, "default", None), None);
        assert_eq!(target(&names[..1], &states[..1], "default", None), None);
    }

    #[test]
    fn detail_names_the_account_and_both_windows() {
        use crate::claude_accounts::profile_state::WindowView;
        let state = ProfileState {
            account: Some("work@example.com (max)".into()),
            five_hour: Some(WindowView {
                percent: 23,
                reset: None,
            }),
            seven_day: Some(WindowView {
                percent: 41,
                reset: None,
            }),
            ..Default::default()
        };
        assert_eq!(
            detail(Some(&state)).as_deref(),
            Some("work@example.com (max) · 5h 23% · week 41%")
        );
        assert_eq!(detail(Some(&ProfileState::default())), None);
    }

    fn accounts() -> Accounts {
        let mut accounts = Accounts::new(true);
        accounts.names = vec!["default".into(), "backup".into(), "work".into()];
        accounts.selected = "work".into();
        accounts
    }

    #[cfg(unix)]
    #[test]
    fn the_key_opens_the_confirmation_and_cycles_without_saving() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let state: crate::monitor::snapshot::MonitorSnapshot = crate::monitor::mock_state().into();
        let mut app = crate::tui::mouse_tests::app_for_accounts_test(accounts());
        let press = |app: &mut crate::tui::MonitorApp| {
            app.handle_key(
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE),
                &state,
            );
            app.accounts.pending.clone().map(|(name, _)| name)
        };
        assert_eq!(press(&mut app).as_deref(), Some("default"));
        assert_eq!(app.accounts.focus, 0);
        assert_eq!(press(&mut app).as_deref(), Some("backup"));
        assert_eq!(press(&mut app).as_deref(), Some("default"));
        assert_eq!(app.accounts.selected, "work");
        let text = crate::tui::tests::buffer_text(&crate::tui::tests::draw(80, 24, |frame| {
            app.accounts.render_confirmation(frame, frame.area())
        }));
        assert!(text.contains("Next launch") && text.contains("default"));
        assert!(text.contains("Enter save  Esc cancel  n next"));
        assert!(app.accounts.confirmation_visible());
    }

    #[cfg(unix)]
    #[test]
    fn the_key_does_nothing_when_read_only_overridden_or_busy_with_an_action() {
        let mut read_only = Accounts::new(false);
        read_only.names = vec!["default".into(), "backup".into()];
        read_only.quick_switch();
        assert!(!read_only.confirming());
        let mut overridden = accounts();
        overridden.overridden = true;
        overridden.quick_switch();
        assert!(!overridden.confirming());
        let mut acting = accounts();
        acting.focus = 1;
        acting.action_key('d');
        acting.quick_switch();
        assert!(acting.pending.is_none() && acting.confirming());
        let mut alone = accounts();
        alone.names.truncate(1);
        alone.selected = "default".into();
        alone.quick_switch();
        assert!(!alone.confirming());
        assert_eq!(alone.status, "No other profile to switch to");
    }
}
