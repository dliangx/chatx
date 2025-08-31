import React, { useState, useEffect, useRef } from "react";
import JoinForm from "./components/JoinForm";
import ChatRoom from "./components/ChatRoom";
import "./App.css";

function App() {
  const [currentView, setCurrentView] = useState("join");
  const [username, setUsername] = useState("");
  const [channel, setChannel] = useState("");
  const [onlineUsers, setOnlineUsers] = useState(new Set());
  const [messages, setMessages] = useState([]);
  const [hasWelcomeMessage, setHasWelcomeMessage] = useState(true);
  const wsRef = useRef(null);

  const handleWebSocketMessage = (
    event,
    currentUsername = username,
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

  const handleJoin = (user, chan) => {
    setUsername(user);
    setChannel(chan);
    setCurrentView("chat");
    setMessages([]);
    setHasWelcomeMessage(true);

    // Connect to WebSocket
    wsRef.current = new WebSocket("ws://127.0.0.1:3000/ws");

    wsRef.current.onopen = () => {
      const joinMessage = {
        username: user,
        channel: chan,
        message: "",
      };
      wsRef.current.send(JSON.stringify(joinMessage));
    };

    wsRef.current.onmessage = (event) => {
      // Use the current user and channel values from the closure
      handleWebSocketMessage(event, user, chan);
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
      const leaveMessage = {
        username: username,
        channel: channel,
        message: "",
      };
      wsRef.current.send(JSON.stringify(leaveMessage));
      wsRef.current.close();
    }

    // Reset state
    setUsername("");
    setChannel("");
    setOnlineUsers(new Set());
    setMessages([]);
    setHasWelcomeMessage(true);
    setCurrentView("join");
  };

  const sendMessage = (message) => {
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

  useEffect(() => {
    return () => {
      if (wsRef.current) {
        wsRef.current.close();
      }
    };
  }, []);

  return (
    <div className="app-container">
      {currentView === "join" ? (
        <JoinForm onJoin={handleJoin} />
      ) : (
        <ChatRoom
          username={username}
          channel={channel}
          onlineUsers={onlineUsers}
          messages={messages}
          hasWelcomeMessage={hasWelcomeMessage}
          onLeave={handleLeave}
          onSendMessage={sendMessage}
        />
      )}
    </div>
  );
}

export default App;
