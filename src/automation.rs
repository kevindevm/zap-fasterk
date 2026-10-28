//! Explicit local automation over a separate authenticated fastframe-instance slot.
use serde::{Deserialize, Serialize};
use std::{
    path::Path,
    sync::{
        Arc, Mutex, Weak,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::Duration,
};

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Fragment {
        request: Box<Request>,
        token: String,
        offset: usize,
    },
    Chats {
        account: String,
        after: Option<String>,
        limit: usize,
    },
    Contacts {
        account: String,
        all: bool,
        after: Option<String>,
        limit: usize,
    },
    Search {
        account: String,
        chat: String,
        text: String,
        before: Option<i64>,
        limit: usize,
    },
    WatchAccount {
        account: String,
        since: i64,
        after: Option<i64>,
        limit: usize,
    },
    SendFiles {
        account: String,
        chat: String,
        request_id: String,
        text: String,
        files: Vec<Attachment>,
    },
    Read {
        account: String,
        chat: String,
        before: Option<(i64, String)>,
        limit: usize,
    },
    Watch {
        account: String,
        chat: String,
        after: Option<(i64, String)>,
        limit: usize,
    },
    Send {
        account: String,
        chat: String,
        request_id: String,
        text: String,
    },
    Status {
        account: String,
        chat: String,
        request_id: String,
    },
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attachment {
    pub path: std::path::PathBuf,
    pub size: u64,
    pub sha256: String,
}
pub const FILE_LIMIT: u64 = 64 * 1024 * 1024;
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AutomationRequest([private])")
    }
}
impl Request {
    pub fn base(&self) -> &Self {
        match self {
            Self::Fragment { request, .. } => request,
            request => request,
        }
    }
    pub fn account(&self) -> &str {
        match self {
            Self::Fragment { request, .. } => request.account(),
            Self::Chats { account, .. }
            | Self::Contacts { account, .. }
            | Self::Search { account, .. }
            | Self::WatchAccount { account, .. }
            | Self::SendFiles { account, .. }
            | Self::Read { account, .. }
            | Self::Watch { account, .. }
            | Self::Send { account, .. }
            | Self::Status { account, .. } => account,
        }
    }
    pub fn chat(&self) -> &str {
        match self {
            Self::Fragment { request, .. } => request.chat(),
            Self::Chats { .. } | Self::Contacts { .. } | Self::WatchAccount { .. } => "",
            Self::Search { chat, .. }
            | Self::SendFiles { chat, .. }
            | Self::Read { chat, .. }
            | Self::Watch { chat, .. }
            | Self::Send { chat, .. }
            | Self::Status { chat, .. } => chat,
        }
    }
    pub fn is_send(&self) -> bool {
        matches!(self, Self::Send { .. } | Self::SendFiles { .. })
    }
    pub fn valid(&self) -> bool {
        if !crate::model::AccountId::is_safe(self.account())
            || self.chat().len() > 256
            || self.chat().contains(['\n', '\r'])
        {
            return false;
        }
        match self {
            Self::Fragment {
                request,
                token,
                offset,
            } => {
                !matches!(
                    request.as_ref(),
                    Self::Fragment { .. }
                        | Self::Send { .. }
                        | Self::SendFiles { .. }
                        | Self::Status { .. }
                ) && request.valid()
                    && valid_id(token)
                    && *offset > 0
                    && *offset < REPLY_LIMIT
            }
            Self::Chats { after, limit, .. } | Self::Contacts { after, limit, .. } => {
                (1..=25).contains(limit) && after.as_ref().is_none_or(|id| id.len() <= 256)
            }
            Self::WatchAccount {
                limit,
                since,
                after,
                ..
            } => (1..=25).contains(limit) && *since >= 0 && after.is_none_or(|id| id >= 0),
            Self::Search { text, limit, .. } => {
                (1..=25).contains(limit) && !text.trim().is_empty() && text.len() <= 1000
            }
            Self::SendFiles {
                request_id,
                text,
                files,
                ..
            } => {
                valid_id(request_id)
                    && text.len() <= 4000
                    && !files.is_empty()
                    && files.len() <= 8
                    && files.iter().all(|file| {
                        file.path.is_absolute()
                            && file.path.to_str().is_some_and(|path| path.len() <= 2048)
                            && file.size <= FILE_LIMIT
                            && file.sha256.len() == 64
                            && file
                                .sha256
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                    })
            }
            Self::Read { limit, .. } | Self::Watch { limit, .. } => (1..=25).contains(limit),
            Self::Send {
                request_id, text, ..
            } => valid_id(request_id) && !text.trim().is_empty() && text.len() <= 4000,
            Self::Status { request_id, .. } => valid_id(request_id),
        }
    }
}
pub fn valid_id(id: &str) -> bool {
    id.len() == 32
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn error(message: &str) -> String {
    serde_json::json!({"error":message}).to_string()
}

/// IPC stays below the dependency's 16 KiB wire limit. Only bounded public DTOs
/// are fragmented, and every fragment re-runs and hashes the original read.
pub const REPLY_LIMIT: usize = 16 * 1024 * 1024;
const FRAGMENT_BYTES: usize = 6 * 1024;
const INLINE_BYTES: usize = 12 * 1024;
struct ReplyStream {
    token: String,
    request: String,
    digest: [u8; 32],
    total: usize,
    next: usize,
    expires: std::time::Instant,
    permit: Arc<Permit>,
}
#[derive(Default)]
struct ReplyStreams(Vec<ReplyStream>);
impl ReplyStreams {
    fn answer(&mut self, pending: &Pending, reply: String) -> String {
        use base64::Engine;
        use sha2::{Digest, Sha256};
        let now = std::time::Instant::now();
        self.0
            .retain(|stream| now < stream.expires && !stream.permit.blocked());
        if pending.blocked() {
            return error("Request authorization was revoked");
        }
        if reply.len() > REPLY_LIMIT {
            return error("Archive page exceeds the bounded 16 MiB reply limit");
        }
        let request = serde_json::to_string(pending.request.base()).unwrap_or_default();
        let digest: [u8; 32] = Sha256::digest(reply.as_bytes()).into();
        let (index, offset) = if let Request::Fragment { token, offset, .. } = &pending.request {
            let Some(index) = self.0.iter().position(|stream| stream.token == *token) else {
                return error("Fragment expired or authorization was revoked");
            };
            let stream = &self.0[index];
            if stream.request != request
                || stream.next != *offset
                || stream.total != reply.len()
                || stream.digest != digest
            {
                // A foreign request cannot invalidate another account's stream.
                if stream.request == request {
                    self.0.remove(index);
                }
                return error("Fragment request changed or archive page became stale");
            }
            (index, *offset)
        } else {
            if reply.len() <= INLINE_BYTES {
                return reply;
            }
            if pending.request.is_send() || matches!(pending.request, Request::Status { .. }) {
                return error("Response too large");
            }
            if self.0.len() >= 32 {
                return error("Fragment queue is full");
            }
            let mut token = [0_u8; 16];
            if getrandom::fill(&mut token).is_err() {
                return error("Could not authorize fragments");
            }
            self.0.push(ReplyStream {
                token: token.iter().map(|byte| format!("{byte:02x}")).collect(),
                request,
                digest,
                total: reply.len(),
                next: 0,
                expires: now + Duration::from_secs(60),
                permit: pending.permit.clone(),
            });
            (self.0.len() - 1, 0)
        };
        let stream = &mut self.0[index];
        let end = (offset + FRAGMENT_BYTES).min(reply.len());
        let fragment = serde_json::json!({"fragment":{"token":stream.token,"offset":offset,"total":stream.total,"digest":base64::engine::general_purpose::STANDARD.encode(digest),"data":base64::engine::general_purpose::STANDARD.encode(&reply.as_bytes()[offset..end])}}).to_string();
        stream.next = end;
        if end == reply.len() {
            self.0.remove(index);
        }
        fragment
    }
}
fn reply_streams() -> &'static Mutex<ReplyStreams> {
    static STREAMS: std::sync::OnceLock<Mutex<ReplyStreams>> = std::sync::OnceLock::new();
    STREAMS.get_or_init(Mutex::default)
}

/// A request may be authorized once; revocation is permanent even after unlocking.
#[derive(Debug, Default)]
pub struct Permit(Mutex<u8>);
impl Permit {
    fn authorize(&self) -> bool {
        let mut state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if *state != 0 {
            return false;
        }
        *state = 1;
        true
    }
    pub fn revoke(&self) {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) = 2;
    }
    pub fn blocked(&self) -> bool {
        *self.0.lock().unwrap_or_else(|p| p.into_inner()) != 1
    }
    /// Enqueue and revocation share one fence: a send is accepted before revocation or refused.
    pub fn execute<T>(&self, action: impl FnOnce() -> T) -> Option<T> {
        let state = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if *state != 1 {
            return None;
        }
        Some(action())
    }
    #[cfg(test)]
    pub fn allowed() -> Self {
        Self(Mutex::new(1))
    }
}
type TrackedPermit = (String, String, Option<u64>, Weak<Permit>);
#[derive(Default)]
pub struct Permits(Mutex<Vec<TrackedPermit>>);
impl Permits {
    pub fn authorize(&self, pending: &Pending) -> bool {
        self.authorize_scope(pending, None)
    }
    pub fn authorize_collection(&self, pending: &Pending, scope: u64) -> bool {
        self.authorize_scope(pending, Some(scope))
    }
    fn authorize_scope(&self, pending: &Pending, scope: Option<u64>) -> bool {
        let mut requests = self.0.lock().unwrap_or_else(|p| p.into_inner());
        if pending.expired() || !pending.permit.authorize() {
            return false;
        }
        requests.push((
            pending.request.account().into(),
            pending.request.chat().into(),
            scope,
            Arc::downgrade(&pending.permit),
        ));
        true
    }
    pub fn sync(&self, allowed: impl Fn(&str, &str) -> bool) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(account, chat, _, weak)| {
                let Some(permit) = weak.upgrade() else {
                    return false;
                };
                if !allowed(account, chat) {
                    permit.revoke();
                }
                !permit.blocked()
            });
    }
    pub fn revoke_all(&self) {
        self.sync(|_, _| false);
    }
    pub fn sync_collection_scopes(&self, scope: impl Fn(&str) -> Option<u64>) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .retain(|(account, _, captured, weak)| {
                let Some(permit) = weak.upgrade() else {
                    return false;
                };
                if captured.is_some_and(|captured| scope(account) != Some(captured)) {
                    permit.revoke();
                }
                !permit.blocked()
            });
    }
}

#[derive(Debug)]
pub struct Pending {
    pub request: Request,
    pub permit: Arc<Permit>,
    pub reply: mpsc::Sender<String>,
    pub cancelled: Arc<AtomicBool>,
    pub locked: Arc<AtomicBool>,
}
impl Pending {
    pub fn blocked(&self) -> bool {
        self.expired() || self.permit.blocked()
    }
    pub fn expired(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst) || self.locked.load(Ordering::SeqCst)
    }
    pub fn answer(&self, reply: String) {
        let reply = if self.blocked() {
            error("Request expired or application locked")
        } else {
            reply_streams()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .answer(self, reply)
        };
        let _ = self.reply.send(if self.blocked() {
            error("Request authorization was revoked")
        } else {
            reply
        });
    }
}
pub type Queue = Arc<Mutex<Vec<Pending>>>;
pub struct Server {
    pub queue: Queue,
    pub locked: Arc<AtomicBool>,
    pub permits: Permits,
    _guard: fastframe_instance::Guard,
}
fn slot(runtime: &Path) -> fastframe_instance::Slot {
    fastframe_instance::Slot::at(runtime.join("automation-v1"), "zap-fasterk-automation-v1")
}
impl Server {
    pub fn start(runtime: &Path, waker: crate::backend::Waker) -> Result<Self, String> {
        let queue = Queue::default();
        let locked = Arc::new(AtomicBool::new(true));
        let pending = queue.clone();
        let privacy = locked.clone();
        let claim=slot(runtime).claim("probe",move|wire|{
            if wire=="probe" { return Some(error("Automation requires account opt-in")); }
            if privacy.load(Ordering::SeqCst) { return Some(error("Application locked")); }
            let Ok(request)=serde_json::from_str::<Request>(wire) else {return Some(error("Invalid request"));};
            if !request.valid(){return Some(error("Invalid request"));}
            let (reply,received)=mpsc::channel(); let cancelled=Arc::new(AtomicBool::new(false));
            let permit=Arc::new(Permit::default());
            let item=Pending{request,permit:permit.clone(),reply,cancelled:cancelled.clone(),locked:privacy.clone()};
            let mut requests=pending.lock().unwrap_or_else(|p|p.into_inner());
            if requests.len()>=32 { return Some(error("Request queue is full")); }
            requests.push(item); drop(requests); waker.wake();
            let answer=received.recv_timeout(Duration::from_secs(3));
            let authorized=!privacy.load(Ordering::SeqCst) && !permit.blocked();
            cancelled.store(true,Ordering::SeqCst);
            if answer.is_ok() && !authorized { return Some(error("Request authorization was revoked")); }
            Some(answer.unwrap_or_else(|_|error("Request timed out; a send may be Unconfirmed. Query its request_id, never resubmit with a new id")))
        });
        match claim {
            fastframe_instance::Claim::First(guard) => Ok(Self {
                queue,
                locked,
                permits: Permits::default(),
                _guard: guard,
            }),
            _ => Err("Automation slot is already occupied".into()),
        }
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.permits.revoke_all();
        reply_streams()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .0
            .clear();
    }
}
pub fn send(runtime: &Path, request: &Request) -> std::io::Result<String> {
    if !request.valid() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Invalid automation request",
        ));
    }
    assemble_reply(request, |current| {
        slot(runtime).send(&serde_json::to_string(current).map_err(std::io::Error::other)?)
    })
}

fn assemble_reply(
    request: &Request,
    mut exchange: impl FnMut(&Request) -> std::io::Result<String>,
) -> std::io::Result<String> {
    use base64::Engine;
    use sha2::{Digest, Sha256};
    fn invalid() -> std::io::Error {
        std::io::Error::other("Invalid or inconsistent automation fragment")
    }
    let mut current = request.clone();
    let mut bytes = Vec::new();
    let mut identity: Option<(String, String, usize)> = None;
    loop {
        let reply = exchange(&current)?;
        let value: serde_json::Value = serde_json::from_str(&reply).map_err(|_| invalid())?;
        let Some(fragment) = value.get("fragment") else {
            // Refusals are delivered through the usual stderr/nonzero CLI path.
            if identity.is_none() || value.get("error").is_some() {
                return Ok(reply);
            }
            return Err(invalid());
        };
        let token = fragment
            .get("token")
            .and_then(|value| value.as_str())
            .ok_or_else(invalid)?;
        let digest = fragment
            .get("digest")
            .and_then(|value| value.as_str())
            .ok_or_else(invalid)?;
        let total = fragment
            .get("total")
            .and_then(|value| value.as_u64())
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(invalid)?;
        let offset = fragment
            .get("offset")
            .and_then(|value| value.as_u64())
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(invalid)?;
        if !valid_id(token) || total == 0 || total > REPLY_LIMIT || offset != bytes.len() {
            return Err(invalid());
        }
        if let Some((old_token, old_digest, old_total)) = &identity {
            if old_token != token || old_digest != digest || *old_total != total {
                return Err(invalid());
            }
        } else {
            if request.is_send()
                || matches!(request, Request::Status { .. } | Request::Fragment { .. })
            {
                return Err(invalid());
            }
            identity = Some((token.to_owned(), digest.to_owned(), total));
            bytes.reserve_exact(total);
        }
        let data = fragment
            .get("data")
            .and_then(|value| value.as_str())
            .ok_or_else(invalid)?;
        if data.len() > FRAGMENT_BYTES.div_ceil(3) * 4 {
            return Err(invalid());
        }
        let data = base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|_| invalid())?;
        if data.is_empty()
            || data.len() > FRAGMENT_BYTES
            || data.len() > total.saturating_sub(bytes.len())
        {
            return Err(invalid());
        }
        bytes.extend_from_slice(&data);
        if bytes.len() == total {
            if base64::engine::general_purpose::STANDARD.encode(Sha256::digest(&bytes)) != digest {
                return Err(invalid());
            }
            let result = String::from_utf8(bytes).map_err(|_| invalid())?;
            serde_json::from_str::<serde_json::Value>(&result).map_err(|_| invalid())?;
            return Ok(result);
        }
        current = Request::Fragment {
            request: Box::new(request.clone()),
            token: token.to_owned(),
            offset: bytes.len(),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fragment_pending(request: Request, permit: Arc<Permit>) -> Pending {
        let (reply, _) = mpsc::channel();
        Pending {
            request,
            permit,
            reply,
            cancelled: Arc::new(AtomicBool::new(false)),
            locked: Arc::new(AtomicBool::new(false)),
        }
    }
    fn large_page() -> String {
        serde_json::json!({"messages":[{"text":"Synthetic 😀 \\ \"\n".repeat(5000)}],"cursor":12})
            .to_string()
    }
    #[test]
    fn full_long_rows_and_aggregate_pages_reassemble_for_every_read_mode() {
        let requests = [
            Request::Read {
                account: "1".into(),
                chat: "fixture".into(),
                before: None,
                limit: 25,
            },
            Request::Search {
                account: "1".into(),
                chat: String::new(),
                text: "fixture".into(),
                before: None,
                limit: 25,
            },
            Request::Watch {
                account: "1".into(),
                chat: "fixture".into(),
                after: None,
                limit: 25,
            },
            Request::WatchAccount {
                account: "1".into(),
                since: 0,
                after: None,
                limit: 25,
            },
            Request::Chats {
                account: "1".into(),
                after: None,
                limit: 25,
            },
            Request::Contacts {
                account: "1".into(),
                all: true,
                after: None,
                limit: 25,
            },
        ];
        for request in requests {
            let key = if matches!(request, Request::Chats { .. }) {
                "chats"
            } else if matches!(request, Request::Contacts { .. }) {
                "contacts"
            } else {
                "messages"
            };
            let page = serde_json::json!({(key):vec![serde_json::json!({"text":"Synthetic 😀 \\ \"\n".repeat(1000)});25],"cursor":12}).to_string();
            let mut streams = ReplyStreams::default();
            let result = assemble_reply(&request, |current| {
                let wire = streams.answer(
                    &fragment_pending(current.clone(), Arc::new(Permit::allowed())),
                    page.clone(),
                );
                assert!(wire.len() <= INLINE_BYTES);
                Ok(wire)
            })
            .unwrap();
            assert_eq!(result, page);
            assert!(streams.0.is_empty());
        }
        let request = Request::Read {
            account: "1".into(),
            chat: "fixture".into(),
            before: None,
            limit: 1,
        };
        let page = large_page();
        let mut streams = ReplyStreams::default();
        assert_eq!(
            assemble_reply(&request, |current| Ok(streams.answer(
                &fragment_pending(current.clone(), Arc::new(Permit::allowed())),
                page.clone()
            )))
            .unwrap(),
            page
        );
    }
    #[test]
    fn midpoint_privacy_revocation_discards_full_page_even_after_unlock() {
        let request = Request::Chats {
            account: "1".into(),
            after: None,
            limit: 25,
        };
        let first = Arc::new(Permit::allowed());
        let mut streams = ReplyStreams::default();
        let mut calls = 0;
        let result = assemble_reply(&request, |current| {
            calls += 1;
            if calls == 2 {
                first.revoke();
            }
            Ok(streams.answer(
                &fragment_pending(
                    current.clone(),
                    if calls == 1 {
                        first.clone()
                    } else {
                        Arc::new(Permit::allowed())
                    },
                ),
                large_page(),
            ))
        })
        .unwrap();
        assert!(
            serde_json::from_str::<serde_json::Value>(&result)
                .unwrap()
                .get("error")
                .is_some()
        );
        assert_eq!(calls, 2);
    }
    #[test]
    fn fragments_reject_stale_foreign_offsets_and_expiry() {
        let request = Request::Read {
            account: "1".into(),
            chat: "fixture".into(),
            before: None,
            limit: 1,
        };
        let mut streams = ReplyStreams::default();
        let page = large_page();
        let first = streams.answer(
            &fragment_pending(request.clone(), Arc::new(Permit::allowed())),
            page.clone(),
        );
        let value: serde_json::Value = serde_json::from_str(&first).unwrap();
        let token = value["fragment"]["token"].as_str().unwrap().to_owned();
        let fragment = Request::Fragment {
            request: Box::new(request.clone()),
            token: token.clone(),
            offset: FRAGMENT_BYTES,
        };
        let foreign = Request::Fragment {
            request: Box::new(Request::Read {
                account: "2".into(),
                chat: "fixture".into(),
                before: None,
                limit: 1,
            }),
            token: token.clone(),
            offset: FRAGMENT_BYTES,
        };
        assert!(
            streams
                .answer(
                    &fragment_pending(foreign, Arc::new(Permit::allowed())),
                    page.clone()
                )
                .contains("error")
        );
        assert_eq!(streams.0.len(), 1);
        assert!(
            streams
                .answer(
                    &fragment_pending(fragment.clone(), Arc::new(Permit::allowed())),
                    format!("{page} ")
                )
                .contains("error")
        );
        assert!(streams.0.is_empty());
        streams.answer(
            &fragment_pending(request.clone(), Arc::new(Permit::allowed())),
            page.clone(),
        );
        let stream = &mut streams.0[0];
        stream.expires = std::time::Instant::now();
        let expired = Request::Fragment {
            request: Box::new(request),
            token: stream.token.clone(),
            offset: FRAGMENT_BYTES,
        };
        assert!(
            streams
                .answer(
                    &fragment_pending(expired, Arc::new(Permit::allowed())),
                    page
                )
                .contains("error")
        );
    }
    #[test]
    fn fragments_are_bounded_and_never_wrap_sends_or_nested_fragments() {
        let send = Request::Send {
            account: "1".into(),
            chat: "fixture".into(),
            request_id: "a".repeat(32),
            text: "fixture".into(),
        };
        let fragment = Request::Fragment {
            request: Box::new(send),
            token: "a".repeat(32),
            offset: FRAGMENT_BYTES,
        };
        assert!(!fragment.valid());
        assert!(
            !Request::Fragment {
                request: Box::new(fragment),
                token: "b".repeat(32),
                offset: FRAGMENT_BYTES
            }
            .valid()
        );
        let request = Request::Chats {
            account: "1".into(),
            after: None,
            limit: 25,
        };
        let result = assemble_reply(&request, |_| {
            Ok(serde_json::json!({"fragment":{"token":"a".repeat(32),"offset":0,"total":REPLY_LIMIT+1,"digest":"fixture","data":"fixture"}}).to_string())
        });
        assert!(result.is_err());
    }
    #[test]
    fn collection_privacy_changes_permanently_revoke_buffered_replies() {
        let registry = Permits::default();
        let (reply, received) = mpsc::channel();
        let pending = Pending {
            request: Request::Chats {
                account: "2".into(),
                after: None,
                limit: 1,
            },
            permit: Arc::new(Permit::default()),
            reply,
            cancelled: Arc::new(AtomicBool::new(false)),
            locked: Arc::new(AtomicBool::new(false)),
        };
        assert!(registry.authorize_collection(&pending, 10));
        registry.sync_collection_scopes(|_| Some(10));
        assert!(!pending.blocked());
        registry.sync_collection_scopes(|_| Some(11));
        registry.sync_collection_scopes(|_| Some(10));
        pending.answer("synthetic private collection".into());
        assert!(pending.blocked());
        assert!(!received.recv().unwrap().contains("private collection"));
    }
    #[test]
    fn opt_out_and_chat_lock_revoke_delayed_replies_permanently() {
        for cause in ["account opt-out", "chat lock", "account removal"] {
            let registry = Permits::default();
            let (reply, received) = mpsc::channel();
            let pending = Pending {
                request: Request::Read {
                    account: "2".into(),
                    chat: "fixture".into(),
                    before: None,
                    limit: 1,
                },
                permit: Arc::new(Permit::default()),
                reply,
                cancelled: Arc::new(AtomicBool::new(false)),
                locked: Arc::new(AtomicBool::new(false)),
            };
            assert!(registry.authorize(&pending));
            assert!(!pending.blocked());
            registry.sync(|_, _| false);
            registry.sync(|_, _| true);
            assert!(pending.blocked(), "{cause} cannot revive after re-enable");
            assert!(!registry.authorize(&pending));
            assert!(
                pending
                    .permit
                    .execute(|| panic!("revoked send must never enqueue"))
                    .is_none()
            );
            pending.answer("synthetic private body".into());
            assert!(!received.recv().unwrap().contains("synthetic private body"));
        }
    }

    #[test]
    fn app_lock_and_unlock_do_not_revive_previous_request_and_final_handler_scrubs_buffered_reply()
    {
        let registry = Permits::default();
        let (reply, received) = mpsc::channel();
        let pending = Pending {
            request: Request::Read {
                account: "2".into(),
                chat: "fixture".into(),
                before: None,
                limit: 1,
            },
            permit: Arc::new(Permit::default()),
            reply,
            cancelled: Arc::new(AtomicBool::new(false)),
            locked: Arc::new(AtomicBool::new(false)),
        };
        assert!(registry.authorize(&pending));
        pending.answer("synthetic private body".into());
        registry.revoke_all();
        pending.locked.store(true, Ordering::SeqCst);
        pending.locked.store(false, Ordering::SeqCst);
        // Even a reply buffered before the lock is withheld by the transport's final authorization check.
        let buffered = received.recv().unwrap();
        let output = if pending.blocked() {
            error("Request authorization was revoked")
        } else {
            buffered
        };
        assert!(!output.contains("synthetic private body"));
    }

    #[test]
    fn requests_are_bounded_and_debug_never_contains_private_body() {
        let request = Request::Send {
            account: "2".into(),
            chat: "fixture".into(),
            request_id: "0123456789abcdef0123456789abcdef".into(),
            text: "synthetic private body".into(),
        };
        assert!(request.valid());
        assert!(!format!("{request:?}").contains("synthetic private body"));
        let oversized = Request::Read {
            account: "2".into(),
            chat: "fixture".into(),
            before: None,
            limit: 26,
        };
        assert!(!oversized.valid());
        assert!(!valid_id("arbitrary"));
    }
    #[test]
    fn cancellation_or_lock_scrubs_reply() {
        let (reply, received) = mpsc::channel();
        let pending = Pending {
            permit: Arc::new(Permit::allowed()),
            request: Request::Status {
                account: "2".into(),
                chat: "fixture".into(),
                request_id: "0123456789abcdef0123456789abcdef".into(),
            },
            reply,
            cancelled: Arc::new(AtomicBool::new(false)),
            locked: Arc::new(AtomicBool::new(true)),
        };
        pending.answer("synthetic private body".into());
        assert!(!received.recv().unwrap().contains("synthetic private body"));
    }
}
