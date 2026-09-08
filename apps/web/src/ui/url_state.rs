//! The dashboard state the URL carries: the filter in force, the page cursor,
//! the open pull request, and the batch being followed. A URL names a state,
//! and a state has one URL, so a view can be shared and survives a refresh —
//! and so does following a batch, which is what the batch is there for. A
//! state also says what reaching it does to the browser history: whether it
//! is an entry of its own, takes the current one's place, or is where the
//! browser already is.

use std::str::FromStr;

use dependaboard_core::{PageCursor, PrFilter, PrKey, valid_batch_id};

/// The slice of the dashboard's state that lives in the query string.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct UrlState {
    pub(crate) filter: PrFilter,
    pub(crate) cursor: Option<String>,
    pub(crate) pr: Option<PrKey>,
    /// The id of the batch the dashboard follows, so a reload follows it
    /// again and a link to it can be shared.
    pub(crate) batch: Option<String>,
}

/// How a state the dashboard has reached goes into the browser history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HistoryMove {
    /// The browser is already at this state's address.
    Stay,
    /// The state takes the current entry's place.
    Replace,
    /// The state is an entry of its own, with the current one behind it.
    Push,
}

// The query parameters, in the order the route lists them.
const VIEW: &str = "view";
const QUERY: &str = "q";
const REPO: &str = "repo";
const UPDATE_TYPE: &str = "type";
const CHECK: &str = "check";
const LABEL: &str = "label";
const DEPENDENCY: &str = "dep";
const AFTER: &str = "after";
const PR: &str = "pr";
const BATCH: &str = "batch";

/// The one value `view` takes; its absence is the "all open" view.
const ATTENTION: &str = "attention";

impl UrlState {
    /// The state a route (`/path?query`) names. A value the dashboard could
    /// not have set — an unknown parameter, a facet value that is not one of
    /// the facet's, a cursor or pull request key that does not parse, a batch
    /// id the dashboard could not have minted — is dropped, so a stale or
    /// hand-edited link still opens the dashboard.
    pub(crate) fn from_route(route: &str) -> Self {
        let query = route
            .split_once('#')
            .map_or(route, |(before_fragment, _)| before_fragment)
            .split_once('?')
            .map_or("", |(_, query)| query);
        let mut state = Self::default();
        for (name, value) in form_urlencoded::parse(query.as_bytes()) {
            let value = value.as_ref();
            let text = || (!value.is_empty()).then(|| value.to_owned());
            let trimmed_text = || (!value.trim().is_empty()).then(|| value.to_owned());
            match name.as_ref() {
                VIEW => state.filter.needs_attention = value == ATTENTION,
                QUERY => state.filter.query = trimmed_text(),
                REPO => push_unique(&mut state.filter.repos, text()),
                UPDATE_TYPE => push_unique(&mut state.filter.update_types, value.parse().ok()),
                CHECK => push_unique(&mut state.filter.check_statuses, value.parse().ok()),
                LABEL => push_unique(&mut state.filter.labels, text()),
                DEPENDENCY => state.filter.dependency = trimmed_text(),
                AFTER => {
                    state.cursor = PageCursor::decode(value).is_ok().then(|| value.to_owned());
                }
                PR => state.pr = PrKey::from_str(value).ok(),
                BATCH => state.batch = valid_batch_id(value).then(|| value.to_owned()),
                _ => {}
            }
        }
        state
    }

    /// The route for this state: `/`, with a query string when anything is
    /// off its default.
    pub(crate) fn to_route(&self) -> String {
        let mut query = form_urlencoded::Serializer::new(String::new());
        if self.filter.needs_attention {
            query.append_pair(VIEW, ATTENTION);
        }
        if let Some(text) = &self.filter.query {
            query.append_pair(QUERY, text);
        }
        for repo in &self.filter.repos {
            query.append_pair(REPO, repo);
        }
        for update_type in &self.filter.update_types {
            query.append_pair(UPDATE_TYPE, &update_type.to_string());
        }
        for status in &self.filter.check_statuses {
            query.append_pair(CHECK, &status.to_string());
        }
        for label in &self.filter.labels {
            query.append_pair(LABEL, label);
        }
        if let Some(dependency) = &self.filter.dependency {
            query.append_pair(DEPENDENCY, dependency);
        }
        if let Some(cursor) = &self.cursor {
            query.append_pair(AFTER, cursor);
        }
        if let Some(pr) = &self.pr {
            query.append_pair(PR, &pr.to_string());
        }
        if let Some(batch) = &self.batch {
            query.append_pair(BATCH, batch);
        }
        let query = query.finish();
        if query.is_empty() {
            "/".to_owned()
        } else {
            format!("/?{query}")
        }
    }

    /// What reaching this state does to the browser history, given the
    /// address the browser is at and whether the drawer has just given up
    /// reading the pull request it was opened on.
    ///
    /// A state the address already names is where the browser is; an address
    /// that carried values the dashboard dropped, or in another order, is
    /// tidied in place. A pull request that could not be read, a search text
    /// refined, or a batch followed or given up takes the current entry's
    /// place: an entry back would fail the same way, step through what was
    /// typed, or put a running batch down. Any other change is a move of its
    /// own.
    pub(crate) fn history_move(&self, current_route: &str, gave_up: bool) -> HistoryMove {
        let previous = Self::from_route(current_route);
        if *self == previous {
            if self.to_route() == current_route {
                HistoryMove::Stay
            } else {
                HistoryMove::Replace
            }
        } else if gave_up || self.refines_search_of(&previous) || self.same_view_as(&previous) {
            HistoryMove::Replace
        } else {
            HistoryMove::Push
        }
    }

    /// Whether this state should take the place of `previous` in the browser
    /// history rather than follow it. Refining the search text does: it
    /// arrives a debounced word at a time, and back should return to before
    /// the search, not step through it. Starting or clearing the search, and
    /// any other change, is a move of its own.
    fn refines_search_of(&self, previous: &Self) -> bool {
        if self.filter.query.is_none() || previous.filter.query.is_none() {
            return false;
        }
        let with_previous_search = Self {
            filter: PrFilter {
                query: previous.filter.query.clone(),
                ..self.filter.clone()
            },
            ..self.clone()
        };
        with_previous_search == *previous
    }

    /// Whether this state shows the same view as `other`: the same filter,
    /// page, and open pull request, whatever batch each follows. The batch a
    /// dashboard follows rides along with the view rather than being a move
    /// of its own: the pill is over every page, and back should not put a
    /// running batch down.
    fn same_view_as(&self, other: &Self) -> bool {
        Self {
            batch: other.batch.clone(),
            ..self.clone()
        } == *other
    }
}

/// Adds `value` to `values` unless it is absent or already there, so a
/// repeated parameter reads as one facet value.
fn push_unique<T: PartialEq>(values: &mut Vec<T>, value: Option<T>) {
    if let Some(value) = value
        && !values.contains(&value)
    {
        values.push(value);
    }
}

#[cfg(test)]
mod tests {
    use dependaboard_core::{CheckStatus, PageCursor, UpdateType};

    use super::*;

    fn cursor() -> String {
        PageCursor {
            updated_at: 1,
            id: "7#9".to_owned(),
        }
        .encode()
    }

    /// A batch id as the dashboard mints them.
    const BATCH: &str = "01926e3a-7c1e-7b7d-9f8b-2b4c6d8e0f1a";

    fn full_state() -> UrlState {
        UrlState {
            filter: PrFilter {
                query: Some("serde json".to_owned()),
                repos: vec!["acme/api".to_owned(), "acme/web".to_owned()],
                update_types: vec![UpdateType::Major, UpdateType::Patch],
                check_statuses: vec![CheckStatus::Failure],
                labels: vec!["rust".to_owned()],
                dependency: Some("serde_json".to_owned()),
                needs_attention: true,
            },
            cursor: Some(cursor()),
            pr: Some(PrKey::new(7, 9)),
            batch: Some(BATCH.to_owned()),
        }
    }

    #[test]
    fn a_state_survives_the_round_trip_through_its_route() {
        let state = full_state();

        let route = state.to_route();

        assert_eq!(UrlState::from_route(&route), state, "{route}");
    }

    /// The route is the shareable form, so its shape is a contract: one
    /// parameter per facet value, in a fixed order, with the characters that
    /// would break a query string encoded.
    #[test]
    fn the_route_lists_the_state_in_a_fixed_order_and_encodes_it() {
        let route = full_state().to_route();

        assert_eq!(
            route,
            format!(
                "/?view=attention&q=serde+json&repo=acme%2Fapi&repo=acme%2Fweb\
                 &type=major&type=patch&check=failure&label=rust&dep=serde_json&after={}&pr=7%239&batch={BATCH}",
                cursor()
            )
        );
    }

    #[test]
    fn the_default_state_is_the_bare_root() {
        assert_eq!(UrlState::default().to_route(), "/");
        assert_eq!(UrlState::from_route("/"), UrlState::default());
        assert_eq!(UrlState::from_route("/?"), UrlState::default());
    }

    /// A link may be stale or hand-edited; whatever in it the dashboard could
    /// not have set is dropped, and the rest still applies.
    #[test]
    fn values_the_dashboard_could_not_have_set_are_dropped() {
        let route = "/?view=bogus&q=+++&repo=&repo=acme%2Fapi&repo=acme%2Fapi&type=huge&type=minor\
                     &check=flaky&label=&label=rust&dep=+&after=not-a-cursor&pr=7&batch=42&utm_source=slack#top";

        let state = UrlState::from_route(route);

        assert_eq!(
            state,
            UrlState {
                filter: PrFilter {
                    repos: vec!["acme/api".to_owned()],
                    update_types: vec![UpdateType::Minor],
                    labels: vec!["rust".to_owned()],
                    ..PrFilter::default()
                },
                cursor: None,
                pr: None,
                batch: None,
            }
        );
    }

    /// The batch id is a Restate workflow key the dashboard will poll by; only
    /// an id the dashboard could have minted, a UUIDv7, is worth asking after.
    #[test]
    fn only_a_batch_id_the_dashboard_could_have_minted_is_read() {
        assert_eq!(
            UrlState::from_route(&format!("/?batch={BATCH}")).batch,
            Some(BATCH.to_owned())
        );
        // A UUIDv4: the right shape, not the right version.
        assert_eq!(
            UrlState::from_route("/?batch=9b2e4c6a-1d3f-4a5b-8c7d-0e1f2a3b4c5d").batch,
            None
        );
        assert_eq!(UrlState::from_route("/?batch=batch-1").batch, None);
    }

    /// The batch a dashboard follows rides along with whatever else it shows,
    /// so a state that differs in the batch alone is the same view.
    #[test]
    fn a_state_differing_only_in_the_batch_is_the_same_view() {
        let without = UrlState::default();
        let with = UrlState {
            batch: Some(BATCH.to_owned()),
            ..UrlState::default()
        };
        assert!(with.same_view_as(&without));
        assert!(without.same_view_as(&with));
        assert!(with.same_view_as(&with));

        let with_facet = UrlState {
            filter: PrFilter {
                update_types: vec![UpdateType::Major],
                ..PrFilter::default()
            },
            ..with.clone()
        };
        assert!(!with_facet.same_view_as(&without));
    }

    /// The `#` in a pull request key is what separates the fragment, so the
    /// key only survives encoded; a raw one is the fragment and names nothing.
    #[test]
    fn the_pull_request_key_is_read_encoded_only() {
        assert_eq!(
            UrlState::from_route("/?pr=7%239").pr,
            Some(PrKey::new(7, 9))
        );
        assert_eq!(UrlState::from_route("/?pr=7#9").pr, None);
    }

    fn searching(text: &str) -> UrlState {
        UrlState {
            filter: PrFilter {
                query: Some(text.to_owned()),
                ..PrFilter::default()
            },
            ..UrlState::default()
        }
    }

    fn failing_checks() -> UrlState {
        UrlState {
            filter: PrFilter {
                check_statuses: vec![CheckStatus::Failure],
                ..PrFilter::default()
            },
            ..UrlState::default()
        }
    }

    /// Whether a state the dashboard has reached is a new history entry,
    /// takes the last one's place, or is where the browser already is. Each
    /// row is the address the browser is at, the state the dashboard is in
    /// now, and whether the drawer has just given a pull request up.
    #[test]
    fn a_history_move_is_decided_from_the_address_and_the_state() {
        let failing = failing_checks();
        let following = UrlState {
            batch: Some(BATCH.to_owned()),
            ..failing.clone()
        };
        let followed_address = format!("/?check=failure&batch={BATCH}");
        let tidied = UrlState {
            filter: PrFilter {
                update_types: vec![UpdateType::Major],
                check_statuses: vec![CheckStatus::Failure],
                ..PrFilter::default()
            },
            ..UrlState::default()
        };

        let table = [
            (
                "the address already names the state",
                "/?check=failure",
                failing.clone(),
                false,
                HistoryMove::Stay,
            ),
            (
                "back from a search, once the dashboard has followed",
                "/",
                UrlState::default(),
                false,
                HistoryMove::Stay,
            ),
            (
                "a link with stray values is tidied",
                "/?utm_source=slack&type=huge&check=failure&type=major",
                tidied,
                false,
                HistoryMove::Replace,
            ),
            (
                "a facet is a move of its own",
                "/",
                failing.clone(),
                false,
                HistoryMove::Push,
            ),
            (
                "starting a search",
                "/",
                searching("ser"),
                false,
                HistoryMove::Push,
            ),
            (
                "refining a search",
                "/?q=ser",
                searching("serde"),
                false,
                HistoryMove::Replace,
            ),
            (
                "clearing a search",
                "/?q=serde",
                UrlState::default(),
                false,
                HistoryMove::Push,
            ),
            (
                "closing a drawer that opened",
                "/?check=failure&pr=7%239",
                failing.clone(),
                false,
                HistoryMove::Push,
            ),
            (
                "a pull request that could not be read",
                "/?check=failure&pr=7%239",
                failing.clone(),
                true,
                HistoryMove::Replace,
            ),
            (
                "following a batch",
                "/?check=failure",
                following,
                false,
                HistoryMove::Replace,
            ),
            (
                "giving a batch up",
                followed_address.as_str(),
                failing,
                false,
                HistoryMove::Replace,
            ),
        ];

        for (case, current_route, next, gave_up, expected) in table {
            assert_eq!(
                next.history_move(current_route, gave_up),
                expected,
                "{case}"
            );
        }
    }

    /// Typing reaches the URL a debounced word at a time. Back should return
    /// to before the search, not step through what was typed, so refining a
    /// search replaces its entry; starting or clearing one is a move of its own.
    #[test]
    fn only_refining_the_search_text_replaces_the_history_entry() {
        assert!(searching("serde").refines_search_of(&searching("ser")));
        assert!(!searching("ser").refines_search_of(&UrlState::default()));
        assert!(!UrlState::default().refines_search_of(&searching("ser")));

        let mut with_facet = searching("serde");
        with_facet.filter.update_types.push(UpdateType::Major);
        assert!(!with_facet.refines_search_of(&searching("ser")));
    }
}
