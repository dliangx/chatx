import React, { createContext, useContext, useState, useEffect } from "react";

const AuthContext = createContext();

export const useAuth = () => {
  const context = useContext(AuthContext);
  if (!context) {
    throw new Error("useAuth must be used within an AuthProvider");
  }
  return context;
};

export const AuthProvider = ({ children }) => {
  const [user, setUser] = useState(null);
  const [token, setToken] = useState(localStorage.getItem("token"));
  const [loading, setLoading] = useState(true);

  // 验证token
  const verifyToken = async (tokenToVerify) => {
    try {
      const response = await fetch(`api/auth/verify?token=${tokenToVerify}`, {
        method: "POST",
      });

      if (response.ok) {
        const userData = await response.json();
        return userData;
      } else {
        return null;
      }
    } catch (error) {
      console.error("Token verification failed:", error);
      return null;
    }
  };

  // 初始化时检查token
  useEffect(() => {
    const initAuth = async () => {
      console.log("Initializing auth, token exists:", !!token);
      if (token) {
        const userData = await verifyToken(token);
        if (userData) {
          console.log("Token verification successful, setting user:", userData);
          setUser(userData);
        } else {
          console.log("Token verification failed, clearing auth");
          localStorage.removeItem("token");
          setToken(null);
        }
      }
      setLoading(false);
    };

    initAuth();
  }, [token]);

  // 登录
  const login = async (username, password) => {
    try {
      const response = await fetch("api/auth/login", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
        },
        body: JSON.stringify({ username, password }),
      });

      const data = await response.json();

      if (response.ok) {
        console.log("Login successful, setting user:", data.user);
        localStorage.setItem("token", data.token);
        setToken(data.token);
        setUser(data.user);
        return { success: true };
      } else {
        return { success: false, error: data.error };
      }
    } catch (error) {
      console.error("Login error:", error);
      return { success: false, error: "Network error. Please try again." };
    }
  };

  // 注册
  const register = async (username, email, password) => {
    try {
      const response = await fetch("api/auth/register", {
        method: "POST",
        headers: {
          "Content-Type": "application/json",
        },
        body: JSON.stringify({ username, email, password }),
      });

      const data = await response.json();

      if (response.ok) {
        console.log("Registration successful, setting user:", data.user);
        localStorage.setItem("token", data.token);
        setToken(data.token);
        setUser(data.user);
        return { success: true };
      } else {
        return { success: false, error: data.error };
      }
    } catch (error) {
      console.error("Register error:", error);
      return { success: false, error: "Network error. Please try again." };
    }
  };

  // 登出
  const logout = () => {
    localStorage.removeItem("token");
    setToken(null);
    setUser(null);
  };

  const value = {
    user,
    token,
    loading,
    login,
    register,
    logout,
    isAuthenticated: !!user,
  };

  // Debug log for authentication state changes
  React.useEffect(() => {
    console.log("Auth state changed:", {
      isAuthenticated: !!user,
      user: user?.username,
    });
  }, [user]);

  return <AuthContext.Provider value={value}>{children}</AuthContext.Provider>;
};
