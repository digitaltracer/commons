//! UI state shared through context. It only mirrors view models the core sends.
use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use proto::model::{ConversationKind, Role};
use proto::view::{
    AccountView, ConnectionState, ConversationView, DraftView, MessageView, SidebarView,
    WorkspaceView,
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
    pub editing: RwSignal<Option<MessageView>>,
    /// Text to put back in a conversation's composer (editing a queued send).
    pub refill: RwSignal<Option<(Uuid, String)>>,
    /// Seconds tick, for reconnect countdowns.
    pub now: RwSignal<f64>,
    /// A one-off message for the whole window, dismissed by the user.
    pub notice: RwSignal<Option<String>>,
    /// An invite link to start the add-account form with.
    pub invite_link: RwSignal<Option<String>>,
    /// Last conversation opened in each workspace.
    pub last: StoredValue<HashMap<Uuid, Uuid>>,
}

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
            refill: RwSignal::new(None),
            now: RwSignal::new(js_sys::Date::now()),
            notice: RwSignal::new(None),
            invite_link: RwSignal::new(None),
            last: StoredValue::new(HashMap::new()),
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
        self.place.set(Some(Place {
            account_id,
            workspace_id: ws,
        }));
        self.mode.set(Mode::Conversation);
        self.selected.set(Some(sel));
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
