use actix::{
    dev::{MessageResponse, OneshotSender},
    prelude::*,
    Recipient,
};

use std::{collections::HashMap, usize};

use log::info;
use redis::streams::{StreamId, StreamInfoStreamReply, StreamReadOptions};
use redis::{
    streams::{StreamKey, StreamReadReply},
    Client, Commands, Connection, RedisResult,
};

use super::WsMessage;

use crate::{
    constants::{BLOCK_MILLIS, MESSAGE_INTERVAL},
    entity::{Event, Platform},
};

/// 用户上线消息,由websocket session发送到redis
/// redis 接收到online
#[derive(Message)]
#[rtype(result = "()")]
pub struct Online {
    /// websocket session id
    pub id: usize,
    /// logined username
    pub name: String,
    /// device
    pub platform: Platform,
    /// `socket` session addr
    pub addr: Recipient<WsMessage>,
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct Offline {
    /// websocket session id
    pub id: usize,
}

/// 审判
#[derive(Message)]
#[rtype(result = "Vec<String>")]
pub struct Trial {
    pub message: String,
    pub receivers: Vec<String>,
}

impl MessageResponse<Redis, Trial> for Vec<String> {
    fn handle(
        self,
        _ctx: &mut <Redis as Actor>::Context,
        tx: Option<OneshotSender<<Trial as Message>::Result>>,
    ) {
        if let Some(tx) = tx {
            let _ = tx.send(self);
        }
    }
}

pub struct Redis {
    cli: Client,
    sessions: HashMap<usize, Recipient<RedisOffline>>,
}

impl Actor for Redis {
    type Context = Context<Self>;
}
impl Redis {
    pub fn new(cli: Client) -> Self {
        Self {
            cli,
            sessions: HashMap::with_capacity(1),
        }
    }

    pub fn key_platform(&self, username: &str) -> String {
        format!("platforms:{}", username)
    }

    pub fn online_users(&self) -> &'static str {
        "online-users"
    }

    pub fn stream_key(&self, username: &str) -> String {
        format!("stream-messages:{}", username)
    }
}

impl Handler<Online> for Redis {
    type Result = ();

    fn handle(&mut self, msg: Online, _ctx: &mut Self::Context) -> Self::Result {
        info!("start creating redis connection for `{}`", &msg.name);

        let mut con = self
            .cli
            .get_connection()
            .expect("get redis connection error");

        let _: RedisResult<String> = con.hset(self.online_users(), msg.id, msg.name.clone());
        let _: RedisResult<Platform> = con.hset(self.key_platform(&msg.name), msg.id, msg.platform);

        let addr = RedisSession::new(msg.id, msg.name, con, msg.addr).start();

        self.sessions.insert(msg.id, addr.recipient());
    }
}

impl Handler<Offline> for Redis {
    type Result = ();

    fn handle(&mut self, msg: Offline, _: &mut Self::Context) -> Self::Result {
        info!("name:{} disconnected, offline redis session", &msg.id);
        if let Some(session_addr) = self.sessions.get(&msg.id) {
            let _ = session_addr.do_send(RedisOffline);
            self.sessions.remove(&msg.id);

            let mut con = self
                .cli
                .get_connection()
                .expect("get redis connection error");

            let username: RedisResult<String> = con.hget(self.online_users(), msg.id);
            if let Ok(username) = username {
                let _: RedisResult<String> = con.hdel(self.online_users(), msg.id);
                let key_platforms = self.key_platform(&username);
                let _: RedisResult<Platform> = con.hdel(key_platforms, msg.id);
            }

            let _: RedisResult<Platform> = con.hget(self.online_users(), msg.id);
        }
    }
}

impl Handler<Trial> for Redis {
    type Result = Vec<String>;

    fn handle(&mut self, msg: Trial, _: &mut Self::Context) -> Self::Result {
        let mut con = self
            .cli
            .get_connection()
            .expect("get redis connection error");
        let event: Result<Event, serde_json::Error> = serde_json::from_str(&msg.message);
        let mut events = vec![];
        if let Ok(event) = event {
            for receiv in &msg.receivers {
                let id: RedisResult<String> =
                    con.xadd(self.stream_key(receiv), "*", &[("event", &event)]);

                if let Ok(id) = id {
                    events.push(id);
                }
            }
        }
        events
    }
}

#[derive(Message)]
#[rtype(result = "()")]
pub struct RedisOffline;
pub struct RedisSession {
    pub id: usize,
    pub name: String,
    stream_name: String,
    pub session_addr: Connection,
    pub websocket_addr: Recipient<WsMessage>,
}

impl Actor for RedisSession {
    type Context = Context<Self>;

    fn started(&mut self, ctx: &mut Self::Context) {
        ctx.run_interval(MESSAGE_INTERVAL, |act, ctx| {
            act.read_messages(ctx);
        });
    }
}

impl Handler<RedisOffline> for RedisSession {
    type Result = ();

    fn handle(&mut self, _: RedisOffline, ctx: &mut Self::Context) -> Self::Result {
        ctx.stop();
    }
}

impl RedisSession {
    pub fn new(
        id: usize,
        name: String,
        connection: Connection,
        websocket_addr: Recipient<WsMessage>,
    ) -> Self {
        Self {
            id,
            name: name.clone(),
            stream_name: format!("stream-messages:{}", &name),
            session_addr: connection,
            websocket_addr,
        }
    }
}

impl RedisSession {
    fn read_messages(&mut self, ctx: &mut Context<Self>) {
        let inf: RedisResult<StreamInfoStreamReply> =
            self.session_addr.xinfo_stream(&self.stream_name);
        // if inf is Err(_), the xadd command have not been execute, no message
        if let Ok(inf) = inf {
            // no message in stream,keep pollings
            if inf.length == 0 {
                return;
            }
            let opts = StreamReadOptions::default().block(BLOCK_MILLIS).count(10);

            // read all messages in the stream
            let ssr: RedisResult<StreamReadReply> =
                self.session_addr
                    .xread_options(&[&self.stream_name], &["0"], opts);
            if let Ok(ssr) = ssr {
                for StreamKey { key, ids } in ssr.keys {
                    let items: Vec<Event> = ids
                        .iter()
                        .map(|t| Event {
                            subject: t.get("subject").unwrap_or_default(),
                            act: t.get("act").unwrap_or_default(),
                            object: t.get("object").unwrap_or_default(),
                        })
                        .collect();
                    let res = serde_json::to_string(&items);
                    if let Ok(res) = res {
                        self.websocket_addr
                            .send(WsMessage(res))
                            .into_actor(self)
                            .then(move |res, act, ctx| {
                                match res {
                                    Ok(_) => {
                                        // remove all the sended messages out from stream
                                        let id_strs: &Vec<&String> =
                                            &ids.iter().map(|StreamId { id, map: _ }| id).collect();
                                        let _: RedisResult<()> =
                                            act.session_addr.xdel(key, id_strs);
                                    }
                                    // something wrong with socket server
                                    _ => ctx.stop(),
                                }
                                fut::ready(())
                            })
                            .wait(ctx);
                    }
                }
            }
        }
    }
}
