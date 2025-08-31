import React, { useState, useEffect } from "react";

function ChannelsList({ onJoinChannel, onBack }) {
  const [channels, setChannels] = useState([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState(null);

  useEffect(() => {
    fetchChannels();
  }, []);

  const fetchChannels = async () => {
    try {
      setLoading(true);
      const response = await fetch("http://127.0.0.1:3000/api/channels");
      if (!response.ok) {
        throw new Error(`HTTP error! status: ${response.status}`);
      }
      const data = await response.json();
      setChannels(data);
      setError(null);
    } catch (err) {
      setError("Failed to fetch channels. Please ensure the backend server is running.");
      console.error("Error fetching channels:", err);
    } finally {
      setLoading(false);
    }
  };

  const handleJoinChannel = (channel) => {
    onJoinChannel(channel);
  };

  const handleRefresh = () => {
    fetchChannels();
  };

  if (loading) {
    return (
      <div className="channels-container">
        <div className="channels-header">
          <h2>Channel List</h2>
          <button onClick={onBack} className="back-button">
            Back
          </button>
        </div>
        <div className="loading">Loading...</div>
      </div>
    );
  }

  if (error) {
    return (
      <div className="channels-container">
        <div className="channels-header">
          <h2>Channel List</h2>
          <button onClick={onBack} className="back-button">
            Back
          </button>
        </div>
        <div className="error-message">
          <p>{error}</p>
          <button onClick={handleRefresh} className="refresh-button">
            Retry
          </button>
        </div>
      </div>
    );
  }

  return (
    <div className="channels-container">
      <div className="channels-header">
        <h2>Channel List</h2>
        <div className="header-buttons">
          <button onClick={handleRefresh} className="refresh-button">
            Refresh
          </button>
          <button onClick={onBack} className="back-button">
            Back
          </button>
        </div>
      </div>

      {channels.length === 0 ? (
        <div className="no-channels">
          <p>No channels available. Please create a new channel.</p>
        </div>
      ) : (
        <div className="channels-list">
          {channels.map((channel) => (
            <div key={channel} className="channel-item">
              <span className="channel-name">{channel}</span>
              <button
                onClick={() => handleJoinChannel(channel)}
                className="join-channel-button"
              >
                Join
              </button>
            </div>
          ))}
        </div>
      )}
    </div>
  );
}

export default ChannelsList;
