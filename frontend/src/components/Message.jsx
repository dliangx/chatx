import React from "react";

const Message = ({ message }) => {
  const generateAvatar = (username) => {
    return username ? username.charAt(0).toUpperCase() : "?";
  };

  const formatTimestamp = (timestamp) => {
    return timestamp.toLocaleTimeString([], {
      hour: "2-digit",
      minute: "2-digit",
    });
  };

  if (message.type === "system") {
    return (
      <div
        className="system-message"
        style={{
          textAlign: "center",
          color: "var(--text-muted)",
          margin: "10px 0",
          fontSize: "0.9rem",
        }}
      >
        {message.content}
      </div>
    );
  }

  const isOwnMessage = message.isOwn;

  return (
    <div
      className={`message-wrapper ${isOwnMessage ? "message-own" : "message-other"}`}
    >
      {/* Avatar for other users (left side) */}
      {!isOwnMessage && (
        <div className="avatar">{generateAvatar(message.username)}</div>
      )}

      <div className="message">
        {/* Username for other users */}
        {!isOwnMessage && (
          <span className="message-username">{message.username}</span>
        )}

        <div className="message-content">
          {message.content}
          <span
            className="message-timestamp"
            style={{
              fontSize: "0.7rem",
              color: "var(--text-muted)",
              marginLeft: "8px",
            }}
          >
            {formatTimestamp(message.timestamp)}
          </span>
        </div>
      </div>

      {/* Avatar for own messages (right side) */}
      {isOwnMessage && (
        <div className="avatar">{generateAvatar(message.username)}</div>
      )}
    </div>
  );
};

export default Message;
