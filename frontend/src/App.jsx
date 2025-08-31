import React, { useState, useEffect, useRef } from "react";
import { AuthProvider, useAuth } from "./context/AuthContext";
import LoginPage from "./components/LoginPage";
import RegisterPage from "./components/RegisterPage";
import JoinForm from "./components/JoinForm";
import ChatRoom from "./components/ChatRoom";
import ChannelsList from "./components/ChannelsList";
import UserProfile from "./components/UserProfile";
import "./App.css";

function AppContent() {
  const [currentView, setCurrentView] = useState("join");
  const [channel, setChannel] = useState("");
  const [onlineUsers, setOnlineUsers] = useState(new Set());
  const [messages, setMessages] = useState([]);
  const [hasWelcomeMessage, setHasWelcomeMessage] = useState(true);
  const [showProfile, setShowProfile] = useState(false);
  const wsRef = useRef(null);

  const { user, isAuthenticated, loading } = useAuth();

  const handleWebSocketMessage = (
    event,
    currentUsername,
    currentChannel = channel,
  ) => {
    try {
      const msg = JSON.parse(event.data);

      if (msg.message_type === "user_list") {
        try {
          const users = JSON.parse(msg.message);
          setOnlineUsers(new Set(users));
        } catch (e) {
          console.error("Error parsing user list:", e);
        }
      } else if (msg.message_type === "system") {
        // Add system message to chat
        setMessages((prev) => [
          ...prev,
          {
            id: Date.now() + Math.random(),
            type: "system",
            content: msg.message,
            timestamp: new Date(),
          },
        ]);
        setHasWelcomeMessage(false);

        // Update user count for join/leave events
        if (msg.message.includes("joined")) {
          const newUser = msg.message.split(" ")[0];
          setOnlineUsers((prev) => new Set([...prev, newUser]));
        } else if (msg.message.includes("left")) {
          const leftUser = msg.message.split(" ")[0];
          setOnlineUsers((prev) => {
            const newSet = new Set(prev);
            newSet.delete(leftUser);
            return newSet;
          });
        }
      } else if (msg.message) {
        // Add regular message to chat
        const isOwnMessage = msg.username === currentUsername;

        // Check if this is a duplicate of our own temporary message
        if (isOwnMessage) {
          // For our own messages, check if we already have a temporary version
          setMessages((prev) => {
            const existingTempIndex = prev.findIndex(
              (m) => m.isTemporary && m.content === msg.message,
            );

            if (existingTempIndex !== -1) {
              // Replace temporary message with server-confirmed one
              return [
                ...prev.slice(0, existingTempIndex),
                {
                  id: prev[existingTempIndex].id, // Keep same ID
                  type: "message",
                  username: msg.username,
                  content: msg.message,
                  timestamp: new Date(),
                  isOwn: true,
                },
                ...prev.slice(existingTempIndex + 1),
              ];
            }

            // If no temporary message found, add as new message
            return [
              ...prev,
              {
                id: Date.now() + Math.random(),
                type: "message",
                username: msg.username,
                content: msg.message,
                timestamp: new Date(),
                isOwn: true,
              },
            ];
          });
        } else {
          // For messages from other users, always add as new
          setMessages((prev) => [
            ...prev,
            {
              id: Date.now() + Math.random(),
              type: "message",
              username: msg.username,
              content: msg.message,
              timestamp: new Date(),
              isOwn: false,
            },
          ]);
        }
        setHasWelcomeMessage(false);
      }
    } catch (e) {
      console.error("Error parsing message:", e);
    }
  };

  const handleJoin = (username, chan) => {
    setChannel(chan);
    setCurrentView("chat");
    setMessages([]);
    setHasWelcomeMessage(true);

    // Connect to WebSocket
    wsRef.current = new WebSocket("ws://127.0.0.1:3000/ws");

    wsRef.current.onopen = () => {
      const joinMessage = {
        username: username,
        channel: chan,
        message: "",
      };
      wsRef.current.send(JSON.stringify(joinMessage));
    };

    wsRef.current.onmessage = (event) => {
      // Use the current user and channel values from the closure
      handleWebSocketMessage(event, username, chan);
    };

    wsRef.current.onerror = (error) => {
      console.error("WebSocket Error:", error);
      alert(
        "Could not connect to the WebSocket server. Please make sure the backend is running.",
      );
    };

    wsRef.current.onclose = () => {
      console.log("WebSocket connection closed");
    };
  };

  const handleLeave = () => {
    if (wsRef.current && wsRef.current.readyState === WebSocket.OPEN) {
      const username = isAuthenticated && user ? user.username : "Guest";
      const leaveMessage = {
        username: username,
        channel: channel,
        message: "",
      };
      wsRef.current.send(JSON.stringify(leaveMessage));
      wsRef.current.close();
    }

    // Reset state
    setChannel("");
    setOnlineUsers(new Set());
    setMessages([]);
    setHasWelcomeMessage(true);
    setCurrentView("join");
  };

  const handleShowChannels = () => {
    setCurrentView("channels");
  };

  const handleJoinFromChannels = (selectedChannel) => {
    // Get username from authenticated user or prompt for guest username
    const username = isAuthenticated && user ? user.username : "Guest";

    // Directly join the selected channel and start chat
    handleJoin(username, selectedChannel);
  };

  const handleBackFromChannels = () => {
    setCurrentView("join");
  };

  const sendMessage = (message) => {
    const username = isAuthenticated && user ? user.username : "Guest";

    // Add message immediately to local state for instant feedback
    const tempMessage = {
      id: Date.now() + Math.random(),
      type: "message",
      username: username,
      content: message,
      timestamp: new Date(),
      isOwn: true,
      isTemporary: true,
    };
    setMessages((prev) => [...prev, tempMessage]);
    setHasWelcomeMessage(false);

    if (wsRef.current && wsRef.current.readyState === WebSocket.OPEN) {
      const messageData = {
        username: username,
        message: message,
        channel: channel,
      };
      wsRef.current.send(JSON.stringify(messageData));
    }
  };

  // Handle authentication state changes
  useEffect(() => {
    console.log("Auth state change detected:", {
      isAuthenticated,
      currentView,
    });

    // If user logs out while in chat, leave the chat
    if (!isAuthenticated && currentView === "chat") {
      console.log("User logged out while in chat, leaving...");
      handleLeave();
    }

    // If user successfully authenticates, redirect to join page
    if (
      isAuthenticated &&
      (currentView === "login" || currentView === "register")
    ) {
      console.log("User authenticated, redirecting to join page...");
      setCurrentView("join");
    }
  }, [isAuthenticated, currentView]);

  useEffect(() => {
    return () => {
      if (wsRef.current) {
        wsRef.current.close();
      }
    };
  }, []);

  // Show loading screen while checking authentication
  if (loading) {
    return (
      <div className="loading-container">
        <div className="loading-spinner"></div>
        <p>Loading...</p>
      </div>
    );
  }

  // Authentication views
  if (currentView === "login") {
    return (
      <LoginPage
        onSwitchToRegister={() => setCurrentView("register")}
        onSwitchToGuest={() => setCurrentView("join")}
      />
    );
  }

  if (currentView === "register") {
    return (
      <RegisterPage
        onSwitchToLogin={() => setCurrentView("login")}
        onSwitchToGuest={() => setCurrentView("join")}
      />
    );
  }

  return (
    <div className="app-container">
      {/* Header with user info and auth buttons */}
      <header className="app-header">
        <div className="app-title">
          <h1>Chat App</h1>
        </div>
        <div className="app-actions">
          {isAuthenticated && user ? (
            <div className="user-section">
              <button
                onClick={() => setShowProfile(true)}
                className="user-button"
              >
                <div className="user-avatar-small">
                  <span className="avatar-text">
                    {user.username.charAt(0).toUpperCase()}
                  </span>
                </div>
                <span className="user-name">{user.username}</span>
              </button>
            </div>
          ) : (
            <div className="auth-buttons">
              <button
                onClick={() => setCurrentView("login")}
                className="auth-button login"
              >
                Sign In
              </button>
              <button
                onClick={() => setCurrentView("register")}
                className="auth-button register"
              >
                Sign Up
              </button>
            </div>
          )}
        </div>
      </header>

      {/* Main content */}
      <main className="app-main">
        {currentView === "join" ? (
          <JoinForm
            onJoin={handleJoin}
            onShowChannels={handleShowChannels}
            onSwitchToLogin={() => setCurrentView("login")}
          />
        ) : currentView === "channels" ? (
          <ChannelsList
            onJoinChannel={handleJoinFromChannels}
            onBack={handleBackFromChannels}
          />
        ) : (
          <ChatRoom
            username={isAuthenticated && user ? user.username : "Guest"}
            channel={channel}
            onlineUsers={onlineUsers}
            messages={messages}
            hasWelcomeMessage={hasWelcomeMessage}
            onLeave={handleLeave}
            onSendMessage={sendMessage}
          />
        )}
      </main>

      {/* User profile modal */}
      {showProfile && <UserProfile onClose={() => setShowProfile(false)} />}
    </div>
  );
}

function App() {
  return (
    <AuthProvider>
      <AppContent />
    </AuthProvider>
  );
}

export default App;
