import React from "react";
import MessageList from "./MessageList";
import MessageInput from "./MessageInput";

const ChatRoom = ({
  username,
  channel,
  onlineUsers,
  messages,
  hasWelcomeMessage,
  onLeave,
  onSendMessage,
}) => {
  const handleSendMessage = (message) => {
    if (message.trim()) {
      onSendMessage(message.trim());
    }
  };

  return (
    <div className="chat-container">
      <div className="chat-header">
        <div className="channel-info">
          <h2 className="channel-title">{channel}</h2>
          <span className="online-count">{onlineUsers.size} online</span>
        </div>
        <button
          className="leave-button"
          onClick={onLeave}
          title="Leave channel"
        >
          <svg
            width="20"
            height="20"
            viewBox="0 0 24 24"
            fill="none"
            xmlns="http://www.w3.org/2000/svg"
          >
            <path
              d="M9 21H5C4.46957 21 3.96086 20.7893 3.58579 20.4142C3.21071 20.0391 3 19.5304 3 19V5C3 4.46957 3.21071 3.96086 3.58579 3.58579C3.96086 3.21071 4.46957 3 5 3H9M16 17L21 12M21 12L16 7M21 12H9"
              stroke="currentColor"
              strokeWidth="2"
              strokeLinecap="round"
              strokeLinejoin="round"
            />
          </svg>
        </button>
      </div>

      <MessageList messages={messages} hasWelcomeMessage={hasWelcomeMessage} />

      <MessageInput onSendMessage={handleSendMessage} />
    </div>
  );
};

export default ChatRoom;
