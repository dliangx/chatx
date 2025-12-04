-- Create users table
CREATE TABLE IF NOT EXISTS users (
    id TEXT PRIMARY KEY,
    username TEXT UNIQUE NOT NULL,
    email TEXT UNIQUE NOT NULL,
    password_hash TEXT NOT NULL,
    created_at DATETIME NOT NULL,
    last_seen DATETIME,
    status TEXT DEFAULT 'offline'
);

-- Create chat_rooms table
CREATE TABLE IF NOT EXISTS chat_rooms (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    room_type TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at DATETIME NOT NULL,
    description TEXT,
    FOREIGN KEY (created_by) REFERENCES users (id)
);

-- Create room_members table
CREATE TABLE IF NOT EXISTS room_members (
    id TEXT PRIMARY KEY,
    room_id TEXT NOT NULL,
    user_id TEXT NOT NULL,
    joined_at DATETIME NOT NULL,
    role TEXT DEFAULT 'member',
    FOREIGN KEY (room_id) REFERENCES chat_rooms (id),
    FOREIGN KEY (user_id) REFERENCES users (id),
    UNIQUE(room_id, user_id)
);

-- Create messages table
CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    room_id TEXT NOT NULL,
    sender_id TEXT NOT NULL,
    content TEXT NOT NULL,
    message_type TEXT DEFAULT 'text',
    created_at DATETIME NOT NULL,
    edited_at DATETIME,
    reply_to TEXT,
    FOREIGN KEY (room_id) REFERENCES chat_rooms (id),
    FOREIGN KEY (sender_id) REFERENCES users (id),
    FOREIGN KEY (reply_to) REFERENCES messages (id)
);

-- Create friendships table
CREATE TABLE IF NOT EXISTS friendships (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL,
    friend_id TEXT NOT NULL,
    status TEXT NOT NULL,
    created_at DATETIME NOT NULL,
    updated_at DATETIME NOT NULL,
    FOREIGN KEY (user_id) REFERENCES users (id),
    FOREIGN KEY (friend_id) REFERENCES users (id),
    UNIQUE(user_id, friend_id)
);
