//! Seed a local SQLite database with sample data so the chat UI has something
//! to render in its three tabs (chats / contacts / discover).
//!
//! Usage:
//!   cargo run -p sqlite --example seed -- [DB_PATH] [ME]
//!
//! Defaults: DB_PATH = ~/.config/p2pchat/default/messages.db, ME = "dliang".
//! The target file is removed first so the seed is idempotent.

use sqlite::conversations::{self, ConversationPatch};
use sqlite::devices;
use sqlite::groups;
use sqlite::messages::{self, NewMessage};
use sqlite::social;
use sqlite::social_feed;
use sqlite::users::{self, UserPatch};
use sqlite::Pool;

const CONTACT_COUNT: i64 = 52;
const CONTACT_BASE: i64 = 101;
const GROUP_BASE: i64 = 300;
const CONV_BASE: i64 = 10000;

const NAMES: &[&str] = &[
    "Alice", "Aaron", "Bob", "Bella", "Carol", "Charlie", "David", "Diana",
    "Emma", "Eric", "Frank", "Fiona", "Grace", "George", "Henry", "Hannah",
    "Ivy", "Isaac", "Jack", "Julia", "Kate", "Kevin", "Leo", "Lily",
    "Mia", "Mark", "Noah", "Nina", "Olivia", "Oscar", "Peter", "Penny",
    "Quinn", "Queenie", "Rose", "Ryan", "Sam", "Sophia", "Tina", "Tom",
    "Uma", "Ulysses", "Victor", "Vera", "Wendy", "Will", "Xavier", "Xena",
    "Yuki", "Yvonne", "Zoe", "Zack",
];

const BIOS: &[&str] = &[
    "打羽毛球、看展",
    "后端工程师",
    "产品经理",
    "设计师",
    "爱旅行",
    "摄影爱好者",
    "程序员",
    "爱读书",
    "健身达人",
    "美食家",
];

const CHAT_TEXTS: &[&str] = &[
    "你好呀", "在吗？", "周末一起去玩吗", "最近怎么样", "项目进度如何",
    "今晚一起吃饭？", "收到", "好的", "明白了", "谢谢",
    "不客气", "明天见", "哈哈哈", "厉害了", "下次再聊",
    "在忙吗", "有空回我一下", "这个方案可以", "我改好了", "辛苦啦",
];

const GROUP_NAMES: &[&str] = &["产品讨论组", "周末活动群", "技术交流群"];

const POST_TEXTS: &[&str] = &[
    "今天天气真好，出去走走",
    "分享一家超好吃的餐厅",
    "周末去爬山了，风景很美",
    "新项目启动了，加油",
    "这本书推荐给大家",
    "刚拍的照片，分享下",
    "最近在学新技能",
    "健身打卡第 30 天",
    "旅行随拍",
    "咖啡配代码，完美",
    "分享一个好用的小工具",
    "看完电影来打个分",
];

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

    let now = sqlite::now_ms();

    // ---- 联系人（120 个）+ 设备 + 好友关系 ----
    for i in 0..CONTACT_COUNT {
        let id = CONTACT_BASE + i;
        let name = NAMES[(i as usize) % NAMES.len()];
        let username = format!("user{id}");
        let bio = BIOS[(i as usize) % BIOS.len()];
        let peer_id = format!("12D3KooW{:08x}", i);
        users::upsert(
            pool,
            id,
            &UserPatch {
                username: Some(username.clone()),
                nickname: Some(name.into()),
                bio: Some(bio.into()),
                ..Default::default()
            },
        )
        .await?;
        devices::upsert(
            pool,
            id,
            &devices::DevicePatch {
                user_id: Some(id),
                peer_id: Some(peer_id.clone()),
                public_key: Some(format!("pk-{username}")),
                ..Default::default()
            },
        )
        .await?;
        social::add(pool, me_id, id).await?;
        if i % 3 == 0 {
            social::follow(pool, me_id, id).await?;
        }
        if i % 5 == 0 {
            social::follow(pool, id, me_id).await?;
        }
    }

    // ---- DM 会话（120 个，每个 2~4 条消息）----
    for i in 0..CONTACT_COUNT {
        let id = CONTACT_BASE + i;
        let peer_id = format!("12D3KooW{:08x}", i);
        let route_key = format!("dm:{id}");
        let msg_count = 2 + (i % 3) as usize;
        seed_dm(
            pool,
            me_id,
            id,
            &peer_id,
            &route_key,
            msg_count,
            now - i * 90_000,
            i as usize,
        )
        .await?;
    }

    // ---- 群组（3 个）----
    for g in 0..3 {
        let group_id = GROUP_BASE + g;
        let name = GROUP_NAMES[g as usize];
        groups::create(pool, group_id, name, me_id, None).await?;
        for m in 0..6 {
            let member_peer = format!("12D3KooW{:08x}", (g * 10 + m) as i64);
            groups::add_member(pool, group_id, &member_peer, groups::ROLE_MEMBER).await?;
        }
        conversations::upsert(
            pool,
            group_id,
            &ConversationPatch {
                type_: Some(1),
                name: Some(name.into()),
                peer_id: None,
                avatar_path: None,
            },
        )
        .await?;
        for k in 0..5 {
            let sender = CONTACT_BASE + (g * 10 + k) as i64;
            let text = CHAT_TEXTS[((g as usize) * 3 + k as usize) % CHAT_TEXTS.len()];
            messages::insert(
                pool,
                &NewMessage {
                    conversation_id: group_id,
                    sender_id: sender,
                    msg_type: messages::MSG_TYPE_TEXT,
                    text_content: Some(text.into()),
                    timestamp: Some(now - (g as i64) * 120_000 - (k as i64) * 60_000),
                    ..Default::default()
                },
            )
            .await?;
        }
    }

    // ---- 置顶一个会话 ----
    let first_conv = conversations::resolve_id(pool, &format!("dm:{}", CONTACT_BASE)).await?.unwrap();
    conversations::set_pinned(pool, first_conv, true).await?;

    // ---- 发现页帖子（120 个）+ 点赞/评论 ----
    for i in 0..CONTACT_COUNT {
        let author = CONTACT_BASE + i;
        let text = POST_TEXTS[(i as usize) % POST_TEXTS.len()];
        let post = social_feed::create_post(pool, author, Some(text), None, 0).await?;
        if i % 2 == 0 {
            social_feed::like(pool, post.id, me_id).await?;
        }
        if i % 3 == 0 {
            social_feed::like(pool, post.id, CONTACT_BASE + ((i + 1) % CONTACT_COUNT)).await?;
        }
        if i % 4 == 0 {
            let commenter = CONTACT_BASE + ((i + 2) % CONTACT_COUNT);
            social_feed::add_comment(pool, post.id, commenter, "真不错！", None).await?;
        }
    }

    Ok(())
}

async fn seed_dm(
    pool: &Pool,
    me_id: i64,
    peer_user_id: i64,
    peer_id: &str,
    route_key: &str,
    msg_count: usize,
    base_ts: i64,
    salt: usize,
) -> anyhow::Result<()> {
    let conv_id = CONV_BASE + (peer_user_id - CONTACT_BASE);
    conversations::upsert(
        pool,
        conv_id,
        &ConversationPatch {
            type_: Some(0),
            name: Some(route_key.into()),
            peer_id: Some(peer_id.into()),
            avatar_path: None,
        },
    )
    .await?;
    for k in 0..msg_count {
        let mine = k % 2 == 1;
        let sender = if mine { me_id } else { peer_user_id };
        let text = CHAT_TEXTS[(salt * 3 + k * 5) % CHAT_TEXTS.len()];
        messages::insert(
            pool,
            &NewMessage {
                conversation_id: conv_id,
                sender_id: sender,
                msg_type: messages::MSG_TYPE_TEXT,
                text_content: Some(text.into()),
                timestamp: Some(base_ts + (k as i64) * 60_000),
                from_me: mine,
                ..Default::default()
            },
        )
        .await?;
    }
    Ok(())
}
