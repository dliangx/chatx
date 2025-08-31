import React, { useState } from "react";
import { useAuth } from "../context/AuthContext";

const JoinForm = ({ onJoin, onShowChannels, onSwitchToLogin }) => {
  const [channel, setChannel] = useState("");
  const [error, setError] = useState("");
  const { user, isAuthenticated } = useAuth();

  const handleChange = (e) => {
    setChannel(e.target.value);
    // Clear error when user starts typing
    if (error) {
      setError("");
    }
  };

  const handleSubmit = (e) => {
    e.preventDefault();
    const trimmedChannel = channel.trim();

    if (isAuthenticated && user && trimmedChannel) {
      // 如果用户已认证，直接使用认证用户的用户名
      onJoin(user.username, trimmedChannel);
    } else if (trimmedChannel) {
      // 如果用户未认证但输入了频道名，则需要先认证
      onSwitchToLogin();
    } else {
      setError("Please enter a channel name.");
    }
  };

  return (
    <div className="join-container">
      <div className="join-card">
        <div className="join-header">
          <h2 className="join-title">Join Chat Room</h2>
          {isAuthenticated && user ? (
            <p className="join-description">
              Welcome back, <strong>{user.username}</strong>! Choose a channel
              to start chatting
            </p>
          ) : (
            <p className="join-description">
              Enter a channel name to start chatting
            </p>
          )}
        </div>

        {error && (
          <div className="error-message">
            <p>{error}</p>
          </div>
        )}

        {isAuthenticated && user && (
          <div className="user-info">
            <div className="user-avatar">
              <span className="avatar-text">
                {user.username.charAt(0).toUpperCase()}
              </span>
            </div>
            <div className="user-details">
              <span className="user-name">{user.username}</span>
              <span className="user-email">{user.email}</span>
            </div>
          </div>
        )}

        <form onSubmit={handleSubmit}>
          <div className="input-group">
            <div className="input-wrapper">
              <svg
                className="input-icon"
                width="20"
                height="20"
                viewBox="0 0 24 24"
                fill="none"
                xmlns="http://www.w3.org/2000/svg"
              >
                <path
                  d="M21 15C21 15.5304 20.7893 16.0391 20.4142 16.4142C20.0391 16.7893 19.5304 17 19 17H7L3 21V5C3 4.46957 3.21071 3.96086 3.58579 3.58579C3.96086 3.21071 4.46957 3 5 3H19C19.5304 3 20.0391 3.21071 20.4142 3.58579C20.7893 3.96086 21 4.46957 21 5V15Z"
                  stroke="currentColor"
                  strokeWidth="2"
                  strokeLinecap="round"
                  strokeLinejoin="round"
                />
              </svg>
              <input
                type="text"
                value={channel}
                onChange={handleChange}
                placeholder="Channel name"
                className="styled-input"
                autoComplete="off"
                autoFocus
              />
            </div>
          </div>

          <button type="submit" className="primary-button">
            <span>Join Channel</span>
            <svg
              width="20"
              height="20"
              viewBox="0 0 24 24"
              fill="none"
              xmlns="http://www.w3.org/2000/svg"
            >
              <path
                d="M5 12H19M19 12L12 5M19 12L12 19"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              />
            </svg>
          </button>

          <button
            type="button"
            onClick={onShowChannels}
            className="secondary-button"
          >
            <span>Browse Channels</span>
            <svg
              width="20"
              height="20"
              viewBox="0 0 24 24"
              fill="none"
              xmlns="http://www.w3.org/2000/svg"
            >
              <path
                d="M8 10L12 14L16 10"
                stroke="currentColor"
                strokeWidth="2"
                strokeLinecap="round"
                strokeLinejoin="round"
              />
            </svg>
          </button>
        </form>
      </div>
    </div>
  );
};

export default JoinForm;
