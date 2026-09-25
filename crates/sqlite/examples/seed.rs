//! Seed a local SQLite database with sample data so the chat UI has something
//! to render in its three tabs (chats / contacts / discover).
//!
//! Usage:
//!   cargo run -p sqlite --example seed -- [DB_PATH] [ME]
//!
//! Defaults: DB_PATH = ~/.config/p2pchat/default/messages.db, ME = "dliang".
//! The target file is removed first so the seed is idempotent.

use sqlite::conversations::{self, ConversationPatch};
use sqlite::groups;
use sqlite::messages::{self, NewMessage};
use sqlite::social;
use sqlite::social_feed;
use sqlite::users::{self, UserPatch};
use sqlite::Pool;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let db_path = args
        .get(1)
        .map(|s| s.as_str())
        .unwrap_or("/Users/liang/.config/p2pchat/default/messages.db");
    let me = args.get(2).map(|s| s.as_str()).unwrap_or("dliang");

    let _ = std::fs::remove_file(db_path);
    let pool = sqlite::open(std::path::Path::new(db_path)).await?;
    seed(&pool, me).await?;
    println!("seeded {db_path} for user {me}");
    Ok(())
}

async fn seed(pool: &Pool, me: &str) -> anyhow::Result<()> {
    let me_id = users::ensure_identity(pool, me).await?;
    users::upsert(
        pool,
        me_id,
        &UserPatch {
            nickname: Some("我".into()),
            bio: Some("这个人很懒，什么都没写".into()),
            ..Default::default()
        },
    )
    .await?;

    // Friends / contacts.
    let friends = [
        (101i64, "alice", "Alice", "打羽毛球、看展"),
        (102, "bob", "Bob", "后端工程师"),
        (103, "carol", "Carol", "产品经理"),
        (104, "dave", "Dave", "设计师"),
    ];
    for (id, username, nickname, bio) in friends {
        users::upsert(
            pool,
            id,
            &UserPatch {
                username: Some(username.into()),
                nickname: Some(nickname.into()),
                bio: Some(bio.into()),
                ..Default::default()
            },
        )
        .await?;
    }

    social::add(pool, me_id, 101).await?;
    social::add(pool, me_id, 102).await?;
    social::add(pool, me_id, 103).await?;

    social::follow(pool, me_id, 101).await?; // 我关注 Alice
    social::follow(pool, 101, me_id).await?; // Alice 回关（互关）
    social::follow(pool, me_id, 102).await?; // 我关注 Bob（单向）

    // DM conversations: `name` = routing key, `peer_id` = peer user id.
    seed_dm(
        pool,
        me_id,
        101,
        "dm:alice",
        &["你好呀", "在吗？", "周末一起去打球吗"],
    )
    .await?;
    seed_dm(pool, me_id, 102, "dm:bob", &["项目进度如何？", "下周三上线"]).await?;
    seed_dm(pool, me_id, 103, "dm:carol", &["今晚一起吃饭？"]).await?;

    // One group conversation.
    let group_id = 200;
    groups::create(pool, group_id, "产品讨论组", me_id, None).await?;
    groups::add_member(pool, group_id, "alice", groups::ROLE_MEMBER).await?;
    groups::add_member(pool, group_id, "bob", groups::ROLE_ADMIN).await?;
    conversations::upsert(
        pool,
        group_id,
        &ConversationPatch {
            type_: Some(1),
            name: Some("产品讨论组".into()),
            peer_id: None,
            avatar_path: None,
        },
    )
    .await?;
    messages::insert(pool, &NewMessage::text(group_id, 101, "新版本需求我整理好了")).await?;
    messages::insert(pool, &NewMessage { conversation_id: group_id, sender_id: me_id, msg_type: messages::MSG_TYPE_TEXT, text_content: Some("收到，我看看".into()), from_me: true, ..Default::default() }).await?;
    messages::insert(pool, &NewMessage::text(group_id, 102, "后端这边排期到下周一")).await?;

    // Pin one conversation so the pinned path is visible.
    let alice_conv = conversations::resolve_id(pool, "dm:alice").await?.unwrap();
    conversations::set_pinned(pool, alice_conv, true).await?;

    // Social feed posts.
    let p1 = social_feed::create_post(pool, 101, Some("今天在西湖边散步，风景真美！"), None, 0).await?;
    let p2 = social_feed::create_post(pool, 102, Some("新项目启动，求关注"), None, 0).await?;
    let p3 = social_feed::create_post(pool, 103, Some("分享一家超好吃的餐厅"), None, 0).await?;
    let _ = p3;
    social_feed::like(pool, p1.id, me_id).await?;
    social_feed::like(pool, p1.id, 102).await?;
    social_feed::like(pool, p2.id, me_id).await?;
    social_feed::add_comment(pool, p1.id, 103, "下次一起去呀", None).await?;

    Ok(())
}

async fn seed_dm(
    pool: &Pool,
    me_id: i64,
    peer_id: i64,
    route_key: &str,
    texts: &[&str],
) -> anyhow::Result<()> {
    let conv_id = conversations::ensure_dm(pool, route_key).await?;
    conversations::upsert(
        pool,
        conv_id,
        &ConversationPatch {
            type_: Some(0),
            name: Some(route_key.into()),
            peer_id: Some(peer_id),
            avatar_path: None,
        },
    )
    .await?;

    let base = sqlite::now_ms();
    for (i, t) in texts.iter().enumerate() {
        let mine = i % 2 == 1;
        let sender = if mine { me_id } else { peer_id };
        messages::insert(
            pool,
            &NewMessage {
                conversation_id: conv_id,
                sender_id: sender,
                msg_type: messages::MSG_TYPE_TEXT,
                text_content: Some((*t).to_string()),
                timestamp: Some(base + i as i64 * 60_000),
                from_me: mine,
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(())
}
