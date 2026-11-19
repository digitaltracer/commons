//! UI state shared through context. It only mirrors view models the core sends.
use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use proto::model::{ConversationKind, Role};
use proto::view::{
    AccountView, ConnectionState, ConversationView, DraftView, MessageView, OpenedInvite,
    SidebarView, StagedFile, UiPrefs, WorkspaceView,
};
use serde_json::json;
use uuid::Uuid;

use crate::ipc;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Selection {
    pub account_id: Uuid,
    pub conversation_id: Uuid,
}

/// The account and workspace the sidebar shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Place {
    pub account_id: Uuid,
    pub workspace_id: Option<Uuid>,
}

/// Where a list was left: its window offset and a row with its distance from the top.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Anchor {
    pub offset: u32,
    pub key: Uuid,
    pub top: f64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Aside {
    None,
    Thread(Uuid),
    Members,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Conversation,
    Search,
}

#[derive(Clone, PartialEq, Debug)]
pub enum Overlay {
    None,
    Switcher,
    /// ⌘K: jump to a conversation or person.
    Jump,
    /// ⌘/: the keyboard shortcuts.
    Shortcuts,
    Prefs(Uuid),
    NewChannel,
    Browse,
    NewDm,
    SignInAgain(Uuid),
    Person {
        account_id: Uuid,
        user_id: Uuid,
    },
    Invite {
        account_id: Uuid,
        workspace_id: Uuid,
    },
    JoinInvite,
    /// Account settings: ways to sign in.
    Account(Uuid),
    /// Members, roles and invites of a workspace (owners and admins).
    Admin {
        account_id: Uuid,
        workspace_id: Uuid,
    },
}

/// Edit on a queued send: the composer discards it and takes its text, or refuses.
#[derive(Clone, Debug, PartialEq)]
pub struct Refill {
    pub account_id: Uuid,
    pub conversation_id: Uuid,
    pub id: Uuid,
    pub body: String,
}

#[derive(Clone, Copy)]
pub struct Ctx {
    pub sidebar: RwSignal<SidebarView>,
    pub selected: RwSignal<Option<Selection>>,
    pub place: RwSignal<Option<Place>>,
    pub presence: RwSignal<HashMap<Uuid, HashSet<Uuid>>>,
    pub drafts: RwSignal<Vec<DraftView>>,
    pub typing: RwSignal<Vec<String>>,
    pub aside: RwSignal<Aside>,
    pub mode: RwSignal<Mode>,
    pub query: RwSignal<String>,
    pub overlay: RwSignal<Overlay>,
    pub adding: RwSignal<bool>,
    /// The message being edited in place, and its text so far.
    pub editing: RwSignal<Option<MessageView>>,
    pub edit_text: RwSignal<String>,
    /// Edits left open in other conversations this session, by conversation.
    pub edits: StoredValue<HashMap<Uuid, (MessageView, String)>>,
    /// Each list's newest message of mine, for ↑ in an empty composer.
    pub last_mine: StoredValue<HashMap<(Uuid, Option<Uuid>), MessageView>>,
    /// A list to scroll back to its newest message (after a send).
    pub repin: RwSignal<Option<(Uuid, Option<Uuid>)>>,
    /// The last open came from the keyboard in the sidebar, so focus stays there.
    pub keyboard_open: StoredValue<bool>,
    /// Lists scrolled away from their newest message, by conversation and thread (this session).
    pub anchors: StoredValue<HashMap<(Uuid, Option<Uuid>), Anchor>>,
    /// The window is focused and visible.
    pub active: RwSignal<bool>,
    /// Display names seen in member lists, by account and user.
    pub names: RwSignal<HashMap<(Uuid, Uuid), String>>,
    /// Pane sizes, kept by the core.
    pub prefs: RwSignal<UiPrefs>,
    /// The window's inner width in CSS px.
    pub win_width: RwSignal<f64>,
    /// A queued send to move back into its conversation's composer, if the composer is empty.
    pub refill: RwSignal<Option<Refill>>,
    /// Seconds tick, for reconnect countdowns.
    pub now: RwSignal<f64>,
    /// A one-off message for the whole window, dismissed by the user.
    pub notice: RwSignal<Option<String>>,
    /// An invite link to start the add-account form with.
    pub invite_link: RwSignal<Option<String>>,
    /// An invite the app was opened with, for the sign-up form or the join panel.
    pub opened_invite: RwSignal<Option<OpenedInvite>>,
    /// Last conversation opened in each workspace.
    pub last: StoredValue<HashMap<Uuid, Uuid>>,
    /// Conversations opened this session, newest first.
    pub recent: StoredValue<Vec<Selection>>,
    /// Files waiting in each composer, by account, conversation and thread, as the core
    /// reports them.
    pub staged: RwSignal<HashMap<ComposerKey, Vec<StagedFile>>>,
    /// Files are being dragged over the window.
    pub dragging: RwSignal<bool>,
    /// A problem staging dropped files, for the composer that took them.
    pub stage_error: RwSignal<Option<(ComposerKey, String)>>,
}

/// A composer: account, conversation, and the thread root it replies in.
pub type ComposerKey = (Uuid, Uuid, Option<Uuid>);

pub fn ctx() -> Ctx {
    expect_context::<Ctx>()
}

impl Ctx {
    pub fn new() -> Ctx {
        Ctx {
            sidebar: RwSignal::new(SidebarView { accounts: vec![] }),
            selected: RwSignal::new(None),
            place: RwSignal::new(None),
            presence: RwSignal::new(HashMap::new()),
            drafts: RwSignal::new(vec![]),
            typing: RwSignal::new(vec![]),
            aside: RwSignal::new(Aside::None),
            mode: RwSignal::new(Mode::Conversation),
            query: RwSignal::new(String::new()),
            overlay: RwSignal::new(Overlay::None),
            adding: RwSignal::new(false),
            editing: RwSignal::new(None),
            edit_text: RwSignal::new(String::new()),
            edits: StoredValue::new(HashMap::new()),
            last_mine: StoredValue::new(HashMap::new()),
            repin: RwSignal::new(None),
            keyboard_open: StoredValue::new(false),
            anchors: StoredValue::new(HashMap::new()),
            active: RwSignal::new(true),
            names: RwSignal::new(HashMap::new()),
            prefs: RwSignal::new(UiPrefs::default()),
            win_width: RwSignal::new(
                web_sys::window()
                    .and_then(|w| w.inner_width().ok())
                    .and_then(|v| v.as_f64())
                    .unwrap_or(1280.0),
            ),
            refill: RwSignal::new(None),
            now: RwSignal::new(js_sys::Date::now()),
            notice: RwSignal::new(None),
            invite_link: RwSignal::new(None),
            opened_invite: RwSignal::new(None),
            last: StoredValue::new(HashMap::new()),
            recent: StoredValue::new(vec![]),
            staged: RwSignal::new(HashMap::new()),
            dragging: RwSignal::new(false),
            stage_error: RwSignal::new(None),
        }
    }

    pub fn account(&self, id: Uuid) -> Option<AccountView> {
        self.sidebar
            .with(|s| s.accounts.iter().find(|a| a.id == id).cloned())
    }

    pub fn workspace(&self, account_id: Uuid, workspace_id: Uuid) -> Option<WorkspaceView> {
        self.sidebar.with(|s| {
            s.accounts
                .iter()
                .find(|a| a.id == account_id)
                .and_then(|a| a.workspaces.iter().find(|w| w.id == workspace_id).cloned())
        })
    }

    /// The workspace holding a conversation, and the conversation.
    pub fn conversation(&self, sel: Selection) -> Option<(WorkspaceView, ConversationView)> {
        self.sidebar.with(|s| {
            let a = s.accounts.iter().find(|a| a.id == sel.account_id)?;
            a.workspaces.iter().find_map(|w| {
                w.conversations
                    .iter()
                    .find(|c| c.id == sel.conversation_id)
                    .map(|c| (w.clone(), c.clone()))
            })
        })
    }

    pub fn me(&self, account_id: Uuid) -> Option<Uuid> {
        self.sidebar.with(|s| {
            s.accounts
                .iter()
                .find(|a| a.id == account_id)
                .map(|a| a.user_id)
        })
    }

    pub fn role(&self, sel: Selection) -> Option<Role> {
        self.conversation(sel).and_then(|(w, _)| w.role)
    }

    pub fn connection(&self, account_id: Uuid) -> ConnectionState {
        self.sidebar.with(|s| {
            s.accounts
                .iter()
                .find(|a| a.id == account_id)
                .map(|a| a.connection.clone())
                .unwrap_or(ConnectionState::Offline)
        })
    }

    pub fn online(&self, account_id: Uuid, user_id: Uuid) -> bool {
        self.connection(account_id) == ConnectionState::Online
            && self
                .presence
                .with(|p| p.get(&account_id).is_some_and(|s| s.contains(&user_id)))
    }

    /// Opens a conversation, switching the sidebar to its workspace.
    pub fn open(&self, account_id: Uuid, conversation_id: Uuid) {
        let sel = Selection {
            account_id,
            conversation_id,
        };
        let ws = self.conversation(sel).map(|(w, _)| w.id);
        if let Some(w) = ws {
            self.last.update_value(|l| {
                l.insert(w, conversation_id);
            });
        }
        self.recent.update_value(|r| {
            r.retain(|x| *x != sel);
            r.insert(0, sel);
            r.truncate(8);
        });
        self.place.set(Some(Place {
            account_id,
            workspace_id: ws,
        }));
        self.mode.set(Mode::Conversation);
        self.selected.set(Some(sel));
    }

    /// The current workspace's conversations in sidebar order: channels, then messages.
    pub fn sidebar_order(&self, account_id: Uuid, workspace_id: Uuid) -> Vec<Selection> {
        let Some(w) = self.workspace(account_id, workspace_id) else {
            return vec![];
        };
        let (channels, dms): (Vec<_>, Vec<_>) =
            w.conversations.iter().partition(|cv| !is_dm(cv.kind));
        channels
            .into_iter()
            .chain(dms)
            .map(|cv| Selection {
                account_id,
                conversation_id: cv.id,
            })
            .collect()
    }

    /// Every unread, unmuted conversation, workspace by workspace in sidebar order.
    pub fn unread_order(&self) -> Vec<Selection> {
        self.workspace_order()
            .into_iter()
            .flat_map(|(a, w)| self.sidebar_order(a, w))
            .filter(|s| {
                self.conversation(*s)
                    .is_some_and(|(_, cv)| (cv.unread > 0 && !cv.muted) || cv.mentions > 0)
            })
            .collect()
    }

    /// Shows a workspace and its last open conversation (or its first).
    pub fn go_to_workspace(&self, account_id: Uuid, workspace_id: Option<Uuid>) {
        self.place.set(Some(Place {
            account_id,
            workspace_id,
        }));
        self.mode.set(Mode::Conversation);
        let Some(w) = workspace_id else {
            self.selected.set(None);
            return;
        };
        let remembered = self.last.with_value(|l| l.get(&w).copied());
        let first = self.workspace(account_id, w).and_then(|ws| {
            remembered
                .filter(|c| ws.conversations.iter().any(|x| x.id == *c))
                .or_else(|| ws.conversations.first().map(|c| c.id))
        });
        match first {
            Some(c) => self.open(account_id, c),
            None => self.selected.set(None),
        }
    }

    /// A person's name from member lists seen so far, or "?" for their avatar.
    pub fn name(&self, account_id: Uuid, user_id: Uuid) -> String {
        self.names.with_untracked(|n| {
            n.get(&(account_id, user_id))
                .cloned()
                .unwrap_or_else(|| "?".into())
        })
    }

    pub fn remember_names(&self, account_id: Uuid, members: &[proto::model::Member]) {
        self.names.update(|n| {
            for m in members {
                n.insert((account_id, m.user_id), m.display_name.clone());
            }
        });
    }

    /// Starts editing one of my messages in its row.
    pub fn edit(&self, m: MessageView) {
        self.edit_text.set(m.body.clone());
        self.editing.set(Some(m));
    }

    /// Every workspace in switcher order, for ⌘1…⌘9.
    pub fn workspace_order(&self) -> Vec<(Uuid, Uuid)> {
        self.sidebar.with(|s| {
            s.accounts
                .iter()
                .flat_map(|a| a.workspaces.iter().map(move |w| (a.id, w.id)))
                .collect()
        })
    }

    pub fn open_dm(&self, account_id: Uuid, workspace_id: Uuid, user_ids: Vec<Uuid>) {
        let this = *self;
        leptos::task::spawn_local(async move {
            let args =
                json!({"accountId": account_id, "workspaceId": workspace_id, "userIds": user_ids});
            match ipc::invoke::<proto::model::Conversation>("open_dm", args).await {
                Ok(c) => {
                    this.overlay.set(Overlay::None);
                    this.open_when_cached(account_id, c.id);
                }
                Err(e) => ipc::warn(e),
            }
        });
    }

    /// Opens a conversation the core is about to learn about from the server.
    pub fn open_when_cached(&self, account_id: Uuid, conversation_id: Uuid) {
        let this = *self;
        let sel = Selection {
            account_id,
            conversation_id,
        };
        if this.conversation(sel).is_some() {
            this.open(account_id, conversation_id);
            return;
        }
        // Waits for the sidebar to include it; gives up quietly after a few seconds.
        let tries = StoredValue::new(0u32);
        let handle = StoredValue::new_local(None::<leptos::prelude::IntervalHandle>);
        if let Ok(h) = set_interval_with_handle(
            move || {
                tries.update_value(|t| *t += 1);
                if this.conversation(sel).is_some() || tries.get_value() > 50 {
                    if this.conversation(sel).is_some() {
                        this.open(account_id, conversation_id);
                    }
                    if let Some(h) = handle.get_value() {
                        h.clear();
                    }
                }
            },
            std::time::Duration::from_millis(100),
        ) {
            handle.set_value(Some(h));
        }
    }
}

pub fn is_dm(kind: ConversationKind) -> bool {
    matches!(kind, ConversationKind::Dm | ConversationKind::GroupDm)
}

/// Where an opened invite link goes: this device's accounts on its server decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InviteRoute {
    /// No account there: a new server, so the sign-up form.
    Register,
    /// One account there: the join panel for it.
    Join(Uuid),
    /// Several: the join panel, which asks which one.
    Pick(Vec<Uuid>),
}

pub fn invite_route(accounts: &[Uuid]) -> InviteRoute {
    match accounts {
        [] => InviteRoute::Register,
        [one] => InviteRoute::Join(*one),
        many => InviteRoute::Pick(many.to_vec()),
    }
}

impl Ctx {
    /// Shows the screen for an invite the app was opened with. It only fills in a form: no
    /// request is made, and the place in the app doesn't change.
    pub fn show_opened_invite(&self, invite: OpenedInvite) {
        let route = invite_route(&invite.accounts);
        self.opened_invite.set(Some(invite));
        match route {
            InviteRoute::Register => {
                self.overlay.set(Overlay::None);
                self.adding.set(true);
            }
            InviteRoute::Join(_) | InviteRoute::Pick(_) => {
                self.adding.set(false);
                self.overlay.set(Overlay::JoinInvite);
            }
        }
    }

    /// Takes the link the app was opened with, if any, and shows its screen.
    pub fn take_opened_invite(self) {
        leptos::task::spawn_local(async move {
            match ipc::invoke::<Option<OpenedInvite>>("take_invite_link", json!({})).await {
                Ok(Some(invite)) => self.show_opened_invite(invite),
                Ok(None) => {}
                Err(e) => self.notice.set(Some(e)),
            }
        });
    }
}

#[cfg(test)]
mod invite_tests {
    use super::{InviteRoute, invite_route};
    use uuid::Uuid;

    #[test]
    fn an_opened_invite_goes_to_sign_up_join_or_a_choice() {
        let (a, b) = (Uuid::from_u128(1), Uuid::from_u128(2));
        assert_eq!(invite_route(&[]), InviteRoute::Register);
        assert_eq!(invite_route(&[a]), InviteRoute::Join(a));
        assert_eq!(invite_route(&[a, b]), InviteRoute::Pick(vec![a, b]));
    }
}
