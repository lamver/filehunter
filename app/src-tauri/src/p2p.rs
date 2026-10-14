//! Связь двух и более компьютеров с FileHunter: прямое зашифрованное соединение (iroh/QUIC).
//! В одной сети компьютеры находят друг друга сами (mDNS) и соединяются напрямую;
//! через интернет — пробивают NAT, а если не вышло, идут через ретранслятор
//! (он видит только зашифрованный трафик).
//!
//! Доверие — по сопряжению: один компьютер выдаёт одноразовый код-приглашение,
//! другой его вводит. Запросы от несопряжённых устройств отклоняются, кроме самого сопряжения.
//! Ключи S3 никуда не передаются: облака другого компьютера смотрятся через него.

use crate::{AppState, Shared};
use filecat_core::l;
use filecat_core::lang::tr;
use iroh::endpoint::{presets, Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter, Manager};

const ALPN: &[u8] = b"filecat/1";
/// Версия протокола: растёт, когда появляются новые запросы.
/// 2 — операции с файлами (копирование, перемещение, удаление по сети).
pub const PROTO: u32 = 4;
/// Больше этого JSON-сообщение быть не может — защита от мусора.
const MAX_MSG: u32 = 64 << 20;
/// Кусок файла за один запрос при скачивании.
pub const CHUNK: u64 = 4 << 20;
const INVITE_TTL: Duration = Duration::from_secs(10 * 60);
const CALL_TIMEOUT: Duration = Duration::from_secs(60);

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub added: i64,
    /// Что ЭТОТ компьютер открыл тому устройству. По умолчанию — ничего.
    #[serde(default)]
    pub share: Share,
}

/// Доступ, который этот компьютер даёт другому устройству.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Share {
    /// "none" — ничего, "view" — смотреть и скачивать, "full" — ещё копировать сюда, перемещать, удалять.
    #[serde(default)]
    pub level: String,
    /// Корни открытых источников: «C:\», «D:\Фото», «s3://Имя».
    #[serde(default)]
    pub roots: Vec<String>,
}

impl Share {
    fn can_view(&self) -> bool {
        matches!(self.level.as_str(), "view" | "full")
    }

    fn can_write(&self) -> bool {
        self.level == "full"
    }

    /// Путь внутри одного из открытых корней.
    pub fn allows(&self, path: &str) -> bool {
        if !self.can_view() {
            return false;
        }
        let p = path.to_lowercase();
        self.roots.iter().any(|r| {
            let r = r.to_lowercase();
            let r = r.trim_end_matches(['\\', '/']);
            p == r || (p.starts_with(r) && p[r.len()..].starts_with(['\\', '/']))
        })
    }
}

pub struct P2p {
    pub endpoint: Endpoint,
    pub name: String,
    peers: Mutex<Vec<Peer>>,
    invites: Mutex<Vec<(String, Instant, Share)>>,
    conns: tokio::sync::Mutex<HashMap<EndpointId, Connection>>,
    online: Mutex<HashMap<String, bool>>,
    /// Версия протокола у каждого устройства (узнаём при проверке связи).
    proto: Mutex<HashMap<String, u32>>,
    peers_path: PathBuf,
    _router: Router,
}

#[derive(Serialize, Deserialize)]
struct Request {
    method: String,
    #[serde(default)]
    args: Value,
    /// Сколько сырых байт идёт следом за запросом (кусок файла для записи).
    #[serde(default)]
    bin: u64,
}

#[derive(Serialize, Deserialize)]
struct Response {
    ok: bool,
    #[serde(default)]
    data: Value,
    #[serde(default)]
    err: String,
    /// Сколько сырых байт идёт следом за ответом (для чтения файлов).
    #[serde(default)]
    bin: u64,
}

fn computer_name() -> String {
    // FILECAT_NAME — своё имя для проверочного экземпляра на том же компьютере
    std::env::var("FILECAT_NAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .or_else(|_| host_name().ok_or(std::env::VarError::NotPresent))
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| tr("Computer", "Компьютер").into())
}

/// Имя компьютера на macOS — как в «Общий доступ» системных настроек.
fn host_name() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        let o = std::process::Command::new("scutil").args(["--get", "ComputerName"]).output().ok()?;
        let n = String::from_utf8_lossy(&o.stdout).trim().to_string();
        if !n.is_empty() {
            return Some(n);
        }
    }
    None
}

fn load_key(path: &std::path::Path) -> SecretKey {
    if let Ok(b) = std::fs::read(path) {
        if let Ok(arr) = <[u8; 32]>::try_from(b.as_slice()) {
            return SecretKey::from_bytes(&arr);
        }
    }
    let k = SecretKey::generate();
    let _ = std::fs::write(path, k.to_bytes());
    k
}

/// Поднимает узел связи. Вызывается один раз при запуске.
pub async fn start(app: AppHandle, st: Shared) -> anyhow::Result<Arc<P2p>> {
    std::fs::create_dir_all(&st.data_dir)?;
    let key = load_key(&st.data_dir.join("node.key"));
    let endpoint = Endpoint::builder(presets::N0)
        .secret_key(key)
        .alpns(vec![ALPN.to_vec()])
        // в одной локальной сети находим друг друга без интернета
        .address_lookup(iroh_mdns_address_lookup::MdnsAddressLookup::builder())
        .bind()
        .await
        .map_err(|e| anyhow::anyhow!("{e} ({e:?})"))?;
    let peers_path = st.data_dir.join("peers.json");
    let peers: Vec<Peer> = std::fs::read(&peers_path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let p2p = Arc::new_cyclic(|weak: &std::sync::Weak<P2p>| {
        let router = Router::builder(endpoint.clone()).accept(ALPN, Handler { app: app.clone(), p2p: weak.clone() }).spawn();
        P2p {
            endpoint: endpoint.clone(),
            name: computer_name(),
            peers: Mutex::new(peers),
            invites: Mutex::new(Vec::new()),
            conns: tokio::sync::Mutex::new(HashMap::new()),
            online: Mutex::new(HashMap::new()),
            proto: Mutex::new(HashMap::new()),
            peers_path,
            _router: router,
        }
    });
    // раз в 20 секунд проверяем, кто из сопряжённых доступен
    let watcher = p2p.clone();
    tauri::async_runtime::spawn(async move {
        let mut tick = 0u32;
        loop {
            watcher.refresh_online(&app).await;
            // копии индекса: раз в пару минут сверяем версии у тех, кто в сети и умеет их отдавать
            if tick % 6 == 0 {
                for p in watcher.peers() {
                    if watcher.is_online(&p.id) && watcher.proto_of(&p.id) >= 4 {
                        if let Err(e) = crate::replica::sync_peer(&app, &st, &watcher, &p.id).await {
                            eprintln!("копия индекса {}: {e}", p.name);
                        }
                    }
                }
            }
            tick = tick.wrapping_add(1);
            tokio::time::sleep(Duration::from_secs(20)).await;
        }
    });
    Ok(p2p)
}

impl P2p {
    pub fn id(&self) -> String {
        self.endpoint.id().to_string()
    }

    pub fn peers(&self) -> Vec<Peer> {
        self.peers.lock().unwrap().clone()
    }

    pub fn is_online(&self, id: &str) -> bool {
        self.online.lock().unwrap().get(id).copied().unwrap_or(false)
    }

    /// Версия протокола устройства (1 — очень старая).
    pub fn proto_of(&self, id: &str) -> u32 {
        self.proto.lock().unwrap().get(id).copied().unwrap_or(1)
    }

    /// На устройстве старая версия FileHunter (не знает новых запросов).
    pub fn is_outdated(&self, id: &str) -> bool {
        self.proto.lock().unwrap().get(id).is_some_and(|v| *v < PROTO)
    }

    pub fn peer_name(&self, id: &str) -> String {
        self.peers.lock().unwrap().iter().find(|p| p.id == id).map(|p| p.name.clone()).unwrap_or_else(|| tr("another computer", "другой компьютер").into())
    }

    fn is_peer(&self, id: &str) -> bool {
        self.peers.lock().unwrap().iter().any(|p| p.id == id)
    }

    fn save_peers(&self) {
        let peers = self.peers.lock().unwrap().clone();
        if let Ok(b) = serde_json::to_vec_pretty(&peers) {
            let _ = std::fs::write(&self.peers_path, b);
        }
    }

    /// `share`: Some — что открыть этому устройству; None — оставить как было (или ничего для нового).
    fn add_peer(&self, id: String, name: String, share: Option<Share>) {
        {
            let mut peers = self.peers.lock().unwrap();
            let old = peers.iter().find(|p| p.id == id).map(|p| p.share.clone());
            peers.retain(|p| p.id != id);
            let share = share.or(old).unwrap_or_default();
            peers.push(Peer { id: id.clone(), name, added: filecat_core::util::unix_now(), share });
        }
        self.online.lock().unwrap().insert(id, true);
        self.save_peers();
    }

    pub fn share_of(&self, id: &str) -> Share {
        self.peers.lock().unwrap().iter().find(|p| p.id == id).map(|p| p.share.clone()).unwrap_or_default()
    }

    /// Что этот компьютер открывает устройству.
    pub fn set_share(&self, id: &str, share: Share) {
        if let Some(p) = self.peers.lock().unwrap().iter_mut().find(|p| p.id == id) {
            p.share = share;
        }
        self.save_peers();
    }

    /// Своё название для сопряжённого устройства (видно только здесь).
    pub fn rename_peer(&self, id: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        if let Some(p) = self.peers.lock().unwrap().iter_mut().find(|p| p.id == id) {
            p.name = name.to_string();
        }
        self.save_peers();
    }

    pub async fn remove_peer(&self, id: &str) {
        self.peers.lock().unwrap().retain(|p| p.id != id);
        self.online.lock().unwrap().remove(id);
        self.save_peers();
        if let Ok(eid) = EndpointId::from_str(id) {
            if let Some(c) = self.conns.lock().await.remove(&eid) {
                c.close(0u32.into(), b"unpaired");
            }
        }
    }

    /// Новый код-приглашение: «FC1-<идентификатор>-<секрет>». Одноразовый, 10 минут.
    /// Кто вводит код, получает доступ к `share` этого компьютера (сам он ничего не открывает).
    pub fn invite(&self, share: Share) -> String {
        let secret: String = (0..10).map(|_| (b'a' + rand::random_range(0..26u8)) as char).collect();
        let mut inv = self.invites.lock().unwrap();
        inv.retain(|(_, t, _)| t.elapsed() < INVITE_TTL);
        inv.push((secret.clone(), Instant::now(), share));
        format!("FC1-{}-{}", self.id(), secret)
    }

    fn take_invite(&self, secret: &str) -> Option<Share> {
        let mut inv = self.invites.lock().unwrap();
        inv.retain(|(_, t, _)| t.elapsed() < INVITE_TTL);
        let i = inv.iter().position(|(s, _, _)| s == secret)?;
        Some(inv.remove(i).2)
    }

    /// Подключение к другому компьютеру по его коду-приглашению.
    pub async fn join(&self, code: &str) -> Result<Peer, String> {
        let parts: Vec<&str> = code.trim().splitn(3, '-').collect();
        let [tag, id, secret] = parts.as_slice() else { return Err(tr("Invalid invitation code", "Неверный код приглашения").into()) };
        if *tag != "FC1" {
            return Err(tr("Invalid invitation code", "Неверный код приглашения").into());
        }
        if *id == self.id() {
            return Err(tr("This is the code of this same computer", "Это код этого же компьютера").into());
        }
        let eid = EndpointId::from_str(id).map_err(|_| tr("Invalid invitation code", "Неверный код приглашения").to_string())?;
        let (data, _) = self.call_raw(eid, "pair", json!({ "secret": secret, "name": self.name })).await?;
        let name = data["name"].as_str().unwrap_or(tr("Computer", "Компьютер")).to_string();
        // вводящий код ничего своего не открывает (если пара уже была — доступ не меняется)
        self.add_peer(id.to_string(), name.clone(), None);
        Ok(Peer { id: id.to_string(), name, added: filecat_core::util::unix_now(), share: self.share_of(id) })
    }

    async fn connection(&self, eid: EndpointId) -> Result<Connection, String> {
        let mut conns = self.conns.lock().await;
        if let Some(c) = conns.get(&eid) {
            if c.close_reason().is_none() {
                return Ok(c.clone());
            }
        }
        let c = tokio::time::timeout(Duration::from_secs(20), self.endpoint.connect(EndpointAddr::from(eid), ALPN))
            .await
            .map_err(|_| {
                let why = tr(
                    "The computer is not responding — it is turned off or FileHunter is not running on it",
                    "Компьютер не отвечает — он выключен или программа на нём не запущена",
                );
                format!("{OFFLINE} {why}")
            })?
            .map_err(|e| format!("{OFFLINE} {}", l!("Could not connect: {e}", "Не удалось соединиться: {e}")))?;
        conns.insert(eid, c.clone());
        Ok(c)
    }

    /// Запрос к другому компьютеру. Возвращает JSON-ответ и сырые байты (если были).
    async fn call_raw(&self, eid: EndpointId, method: &str, args: Value) -> Result<(Value, Vec<u8>), String> {
        self.call_raw_bin(eid, method, args, Vec::new()).await
    }

    async fn call_raw_bin(&self, eid: EndpointId, method: &str, args: Value, data: Vec<u8>) -> Result<(Value, Vec<u8>), String> {
        let fut = async {
            let conn = self.connection(eid).await?;
            let (mut send, mut recv) = match conn.open_bi().await {
                Ok(s) => s,
                Err(_) => {
                    // соединение устарело — пробуем один раз заново
                    self.conns.lock().await.remove(&eid);
                    self.connection(eid).await?.open_bi().await.map_err(|e| e.to_string())?
                }
            };
            write_msg(&mut send, &Request { method: method.into(), args, bin: data.len() as u64 }).await?;
            if !data.is_empty() {
                send.write_all(&data).await.map_err(|e| e.to_string())?;
            }
            send.finish().map_err(|e| e.to_string())?;
            let resp: Response = read_msg(&mut recv).await?;
            if !resp.ok {
                return Err(resp.err);
            }
            let mut bin = vec![0u8; resp.bin as usize];
            if resp.bin > 0 {
                recv.read_exact(&mut bin).await.map_err(|e| e.to_string())?;
            }
            Ok((resp.data, bin))
        };
        tokio::time::timeout(CALL_TIMEOUT, fut)
            .await
            .map_err(|_| format!("{OFFLINE} {}", tr("The computer is taking too long to respond", "Компьютер слишком долго не отвечает")))?
    }

    /// Запрос без локальной проверки сопряжения — только для самопроверки: другой компьютер
    /// должен сам отказать несопряжённому.
    pub async fn call_unchecked(&self, peer: &str, method: &str) -> Result<Value, String> {
        let eid = EndpointId::from_str(peer).map_err(|e| e.to_string())?;
        self.call_raw(eid, method, Value::Null).await.map(|r| r.0)
    }

    pub async fn call(&self, peer: &str, method: &str, args: Value) -> Result<(Value, Vec<u8>), String> {
        self.call_with(peer, method, args, Vec::new()).await
    }

    /// Запрос с данными (кусок файла для записи на том компьютере).
    pub async fn call_with(&self, peer: &str, method: &str, args: Value, data: Vec<u8>) -> Result<(Value, Vec<u8>), String> {
        if !self.is_peer(peer) {
            return Err(not_paired());
        }
        let eid = EndpointId::from_str(peer).map_err(|e| e.to_string())?;
        // известные ошибки того компьютера — на языке этого (метка остаётся для кода)
        let r = self.call_raw_bin(eid, method, args, data).await.map_err(|e| {
            if is_unknown_request(&e) {
                let name = self.peer_name(peer);
                let why = l!(
                    "«{name}» has an old version of FileHunter — update it there to make this work",
                    "На компьютере «{name}» старая версия FileHunter — обновите её там, чтобы это работало"
                );
                format!("{UNKNOWN_REQUEST} {why}")
            } else if is_no_access(&e) {
                format!("{NO_ACCESS} {}", tr("Access denied: this computer did not share this with you", OLD_NO_ACCESS))
            } else if is_no_write(&e) {
                format!("{NO_WRITE} {}", tr("Access denied: this computer shared this with you for viewing only", OLD_NO_WRITE))
            } else {
                e
            }
        });
        let ok = r.is_ok() || r.as_ref().is_err_and(|e| !e.contains(OFFLINE));
        self.online.lock().unwrap().insert(peer.to_string(), ok);
        r
    }

    async fn refresh_online(&self, app: &AppHandle) {
        let mut changed = false;
        for p in self.peers() {
            let was = self.is_online(&p.id);
            let reply = match EndpointId::from_str(&p.id) {
                Ok(eid) => tokio::time::timeout(Duration::from_secs(10), self.call_raw(eid, "ping", Value::Null)).await.ok().and_then(|r| r.ok()),
                Err(_) => None,
            };
            let now = reply.is_some();
            if let Some((v, _)) = reply {
                // старые версии отвечали на ping пустым ответом — это протокол 1
                let proto = v["proto"].as_u64().unwrap_or(1) as u32;
                let was_proto = self.proto.lock().unwrap().insert(p.id.clone(), proto);
                changed |= was_proto != Some(proto);
            }
            self.online.lock().unwrap().insert(p.id.clone(), now);
            changed |= was != now;
        }
        if changed {
            let _ = app.emit("peers-changed", ());
        }
    }
}

async fn write_msg<T: Serialize>(send: &mut SendStream, msg: &T) -> Result<(), String> {
    let b = serde_json::to_vec(msg).map_err(|e| e.to_string())?;
    send.write_all(&(b.len() as u32).to_be_bytes()).await.map_err(|e| e.to_string())?;
    send.write_all(&b).await.map_err(|e| e.to_string())
}

async fn read_msg<T: for<'de> Deserialize<'de>>(recv: &mut RecvStream) -> Result<T, String> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.map_err(|e| e.to_string())?;
    let len = u32::from_be_bytes(len);
    if len > MAX_MSG {
        return Err(tr("Message too large", "слишком большое сообщение").into());
    }
    let mut b = vec![0u8; len as usize];
    recv.read_exact(&mut b).await.map_err(|e| e.to_string())?;
    serde_json::from_slice(&b).map_err(|e| e.to_string())
}

// ---------- приём запросов от других компьютеров ----------

#[derive(Clone)]
struct Handler {
    app: AppHandle,
    p2p: std::sync::Weak<P2p>,
}

impl std::fmt::Debug for Handler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("filecat-handler")
    }
}

impl ProtocolHandler for Handler {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id().to_string();
        while let Ok((mut send, mut recv)) = connection.accept_bi().await {
            let (app, p2p, remote) = (self.app.clone(), self.p2p.clone(), remote.clone());
            tauri::async_runtime::spawn(async move {
                let Some(p2p) = p2p.upgrade() else { return };
                let Ok(req) = read_msg::<Request>(&mut recv).await else { return };
                // данные запроса (кусок файла) — не больше одного куска с запасом
                if req.bin > 2 * crate::fsops::CHUNK {
                    return;
                }
                let mut data = vec![0u8; req.bin as usize];
                if req.bin > 0 && recv.read_exact(&mut data).await.is_err() {
                    return;
                }
                let paired = p2p.is_peer(&remote);
                let res = if req.method == "pair" {
                    pair(&p2p, &remote, &req.args)
                } else if !paired {
                    Err(not_paired())
                } else {
                    p2p.online.lock().unwrap().insert(remote.clone(), true);
                    serve(&app, &p2p.share_of(&remote), &req.method, req.args, data).await
                };
                let (resp, bin) = match res {
                    Ok((data, bin)) => (Response { ok: true, data, err: String::new(), bin: bin.len() as u64 }, bin),
                    Err(err) => (Response { ok: false, data: Value::Null, err, bin: 0 }, vec![]),
                };
                if write_msg(&mut send, &resp).await.is_ok() && !bin.is_empty() {
                    let _ = send.write_all(&bin).await;
                }
                let _ = send.finish();
                if req.method == "pair" {
                    let _ = app.emit("peers-changed", ());
                }
            });
        }
        Ok(())
    }
}

fn pair(p2p: &P2p, remote: &str, args: &Value) -> Result<(Value, Vec<u8>), String> {
    let secret = args["secret"].as_str().unwrap_or_default();
    let Some(share) = p2p.take_invite(secret) else {
        return Err(tr("The invitation code is invalid or expired — create a new one", "Код приглашения неверный или устарел — создайте новый").into());
    };
    let name = args["name"].as_str().unwrap_or(tr("Computer", "Компьютер")).to_string();
    // открываем ему ровно то, что выбрали при создании кода
    p2p.add_peer(remote.to_string(), name, Some(share));
    Ok((json!({ "name": p2p.name }), vec![]))
}

fn arg<T: for<'de> Deserialize<'de>>(a: &Value, k: &str) -> Result<T, String> {
    serde_json::from_value(a.get(k).cloned().unwrap_or(Value::Null)).map_err(|e| l!("Argument {k}: {e}", "аргумент {k}: {e}"))
}

fn val<T: Serialize>(v: T) -> Result<(Value, Vec<u8>), String> {
    serde_json::to_value(v).map(|v| (v, vec![])).map_err(|e| e.to_string())
}

fn not_paired() -> String {
    tr("The device is not paired", "Устройство не сопряжено").into()
}

// Метки ошибок: одинаковые на любом языке, по ним ошибку узнаёт код — здесь и на другом
// компьютере. За меткой — текст для человека; показывая ошибку, метку убирают (`shown`).
/// Запросы, которые другой компьютер может выполнить здесь: те же команды, что и в окне.
pub const NO_ACCESS: &str = "[no-access]";
pub const NO_WRITE: &str = "[no-write]";
pub const UNKNOWN_REQUEST: &str = "[unknown-request]";
/// Компьютер не отвечает или с ним не удалось соединиться. Метка только для этого
/// компьютера — по сети не уходит.
const OFFLINE: &str = "[offline]";
// Прежние версии присылают те же ошибки без меток, по-русски. Этот же текст мы ставим
// после метки: прежние версии узнают ошибку по нему, новые — по метке (и пишут её
// на своём языке, см. `call_with`).
const OLD_NO_ACCESS: &str = "Нет доступа: этот компьютер вам это не открывал";
const OLD_NO_WRITE: &str = "Нет доступа: этот компьютер открыл вам только просмотр";
const OLD_UNKNOWN_REQUEST: &str = "неизвестный запрос";

pub fn is_no_access(e: &str) -> bool {
    e.contains(NO_ACCESS) || e.contains(OLD_NO_ACCESS)
}

fn is_no_write(e: &str) -> bool {
    e.contains(NO_WRITE) || e.contains(OLD_NO_WRITE)
}

pub fn is_unknown_request(e: &str) -> bool {
    e.contains(UNKNOWN_REQUEST) || e.starts_with(OLD_UNKNOWN_REQUEST)
}

/// Ошибка для человека: без меток.
pub fn shown(e: &str) -> String {
    [NO_ACCESS, NO_WRITE, UNKNOWN_REQUEST, OFFLINE].iter().fold(e.to_string(), |s, m| s.replace(&format!("{m} "), ""))
}

/// Ошибки, которые уходят другому компьютеру: метка и прежний текст.
fn no_access() -> String {
    format!("{NO_ACCESS} {OLD_NO_ACCESS}")
}

fn no_write() -> String {
    format!("{NO_WRITE} {OLD_NO_WRITE}")
}

fn unknown_request(method: &str) -> String {
    format!("{UNKNOWN_REQUEST} {OLD_UNKNOWN_REQUEST}: {method}")
}

/// Запросы, которые другой компьютер может выполнить здесь — только в пределах того,
/// что ему открыли (`share`). Проверка здесь, на стороне открывшего.
async fn serve(app: &AppHandle, share: &Share, method: &str, a: Value, data: Vec<u8>) -> Result<(Value, Vec<u8>), String> {
    let st = app.state::<Shared>();
    // какой корень у источника с номером seg
    let seg_root = |a: &Value| -> Result<String, String> {
        let seg: u16 = arg(a, "seg")?;
        st.index.read().unwrap().segments.get(seg as usize).map(|s| s.info.root.clone()).ok_or_else(|| tr("Source not found", "источник не найден").to_string())
    };
    let need_path = |key: &str, write: bool| -> Result<(), String> {
        let p: String = arg(&a, key)?;
        if !share.allows(&p) {
            return Err(no_access());
        }
        if write && !share.can_write() {
            return Err(no_write());
        }
        Ok(())
    };
    match method {
        "ping" => {}
        "sources" => {
            let list: Vec<_> = crate::sources_now(st.clone()).into_iter().filter(|s| share.allows(&s.root)).collect();
            return val(list);
        }
        "browse" | "search_folder" | "s3_refresh" => {
            if !share.allows(&seg_root(&a)?) {
                return Err(no_access());
            }
        }
        "browse_live" | "preview" => need_path("path", false)?,
        "locate" | "fs_tree" | "fs_read" | "fs_exists" | "read" => need_path("path", false)?,
        "fs_mkdir" | "fs_delete" | "fs_write" | "fs_write_done" | "fs_write_abort" => need_path("path", true)?,
        "source_rename" | "source_remove" => need_path("root", true)?,
        "fs_rename" => {
            need_path("from", true)?;
            need_path("to", true)?;
        }
        "replica_list" => {
            if !share.can_view() {
                return Err(no_access());
            }
        }
        "replica_get" => need_path("root", false)?,
        "trash" => {
            let files: Vec<Value> = arg(&a, "files")?;
            if !share.can_write() || !files.iter().all(|f| f["path"].as_str().is_some_and(|p| share.allows(p))) {
                return Err(no_write());
            }
        }
        _ => return Err(unknown_request(method)),
    }
    // операции с файлами — в отдельном потоке: диск и S3 могут отвечать долго
    let fs = |f: Box<dyn FnOnce(&AppState) -> Result<(Value, Vec<u8>), String> + Send>| {
        let st = st.inner().clone();
        async move { tauri::async_runtime::spawn_blocking(move || f(&st)).await.map_err(|e| e.to_string())? }
    };
    use crate::fsops;
    match method {
        "fs_tree" => {
            let path: String = arg(&a, "path")?;
            fs(Box::new(move |st| val(fsops::list_tree(st, &path)?))).await
        }
        "fs_read" => {
            let (path, offset, len): (String, u64, u64) = (arg(&a, "path")?, arg(&a, "offset")?, arg(&a, "len")?);
            fs(Box::new(move |st| Ok((Value::Null, fsops::read(st, &path, offset, len.min(fsops::CHUNK))?)))).await
        }
        "fs_exists" => {
            let path: String = arg(&a, "path")?;
            fs(Box::new(move |st| val(fsops::exists(st, &path)?))).await
        }
        "fs_mkdir" => {
            let path: String = arg(&a, "path")?;
            fs(Box::new(move |st| val(fsops::mkdir(st, &path)?))).await
        }
        "fs_delete" => {
            let (path, forever): (String, bool) = (arg(&a, "path")?, arg(&a, "forever")?);
            fs(Box::new(move |st| val(fsops::delete(st, &path, forever)?))).await
        }
        // копия индекса: какие корни открыты и какой версии; снимок корня кусками
        "replica_list" => {
            let version = st.generation.load(std::sync::atomic::Ordering::Relaxed);
            val(share.roots.iter().map(|r| json!({ "root": r, "version": version })).collect::<Vec<_>>())
        }
        "replica_get" => {
            let (root, offset): (String, u64) = (arg(&a, "root")?, arg(&a, "offset")?);
            if !share.roots.iter().any(|r| r.eq_ignore_ascii_case(&root)) {
                return Err(no_access());
            }
            let version = st.generation.load(std::sync::atomic::Ordering::Relaxed);
            fs(Box::new(move |st| {
                let b = crate::replica::export(st, &root, version, offset == 0)?;
                let from = (offset as usize).min(b.len());
                let to = (from + CHUNK as usize).min(b.len());
                Ok((json!({ "total": b.len() }), b[from..to].to_vec()))
            }))
            .await
        }
        "fs_rename" => {
            let (from, to): (String, String) = (arg(&a, "from")?, arg(&a, "to")?);
            fs(Box::new(move |_| val(fsops::rename(&from, &to)?))).await
        }
        "fs_write" => {
            let (path, offset): (String, u64) = (arg(&a, "path")?, arg(&a, "offset")?);
            fs(Box::new(move |_| val(fsops::recv_chunk(&path, offset, &data)?))).await
        }
        "fs_write_done" => {
            let path: String = arg(&a, "path")?;
            fs(Box::new(move |st| val(fsops::recv_done(st, &path)?))).await
        }
        "fs_write_abort" => {
            let path: String = arg(&a, "path")?;
            fsops::recv_abort(&path);
            val(())
        }
        "ping" => val(json!({ "proto": PROTO, "version": env!("CARGO_PKG_VERSION") })),
        "sources" => val(crate::sources_now(st)),
        "browse" => val(crate::browse(st, arg(&a, "seg")?, arg(&a, "dir")?, arg(&a, "sort")?, arg(&a, "desc")?)?),
        "browse_live" => val(crate::browse_live(st, arg(&a, "root")?, arg(&a, "path")?).await?),
        "search_folder" => val(crate::search_folder(st, arg(&a, "seg")?, arg(&a, "dir")?, arg(&a, "query")?)?),
        "locate" => val(crate::locate(st, arg(&a, "path")?)?),
        "s3_refresh" => val(crate::s3_refresh(st, arg(&a, "seg")?, arg(&a, "dir")?).await?),
        "preview" => val(crate::preview(st, arg(&a, "root")?, arg(&a, "path")?, arg(&a, "size")?).await?),
        "read" => read_chunk(&st, arg(&a, "path")?, arg(&a, "offset")?).await,
        "trash" => val(crate::trash_files(st, arg(&a, "files")?)),
        "source_rename" => val(crate::source_rename(app.clone(), st, arg(&a, "root")?, arg(&a, "name")?)?),
        "source_remove" => val(crate::source_remove(st, arg(&a, "root")?)?),
        _ => Err(unknown_request(method)),
    }
}

/// Кусок локального файла для скачивания на другой компьютер.
async fn read_chunk(_st: &AppState, path: String, offset: u64) -> Result<(Value, Vec<u8>), String> {
    tauri::async_runtime::spawn_blocking(move || {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = filecat_core::throttle::open_shared(std::path::Path::new(&path), true).map_err(|e| e.to_string())?;
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        f.seek(SeekFrom::Start(offset)).map_err(|e| e.to_string())?;
        let mut buf = Vec::with_capacity(CHUNK as usize);
        f.take(CHUNK).read_to_end(&mut buf).map_err(|e| e.to_string())?;
        Ok((json!({ "size": size }), buf))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_allows_only_opened_roots() {
        let s = Share { level: "view".into(), roots: vec![r"D:\Фото".into(), "s3://Backup".into()] };
        assert!(s.allows(r"D:\Фото") && s.allows(r"d:\фото\2025\a.jpg"));
        assert!(!s.allows(r"D:\Фото-личное\a.jpg"), "похожий путь — не открыт");
        assert!(!s.allows(r"D:\") && !s.allows(r"C:\Users"));
        assert!(s.allows("s3://Backup/photos/x.jpg") && !s.allows("s3://Archive/x"));
        assert!(!s.can_write());
        let none = Share::default();
        assert!(!none.allows(r"D:\Фото"), "по умолчанию — ничего");
        let full = Share { level: "full".into(), roots: vec![r"C:\".into()] };
        assert!(full.allows(r"C:\Users\a.txt") && full.can_write());
    }
}

