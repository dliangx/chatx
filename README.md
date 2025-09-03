# Rust Chat App

A modern real-time chat application built with a Rust backend and React frontend, featuring user authentication, real-time messaging, direct messages, group chats, and message history.

This project was bootstrapped and enhanced with the help of a Gemini agent.

## ✨ Features

### Core Chat Features
- **Real-time messaging** with WebSockets
- **Direct messages** - Private one-on-one conversations
- **Group chats** - Create or join different chat rooms
- **Message history** - Persistent chat history with SQLite database
- **Online status** - View user availability in real-time
- **System notifications** for user join/leave events
- **Browse chat rooms** and conversations

### User Authentication & Social Features 🔐
- **User Registration**: Create new accounts with username, email, and password
- **User Login**: Secure authentication with JWT tokens
- **Password Security**: Bcrypt encryption for password storage
- **Token-based Auth**: JWT tokens with automatic expiration
- **User Profiles**: View and manage user information
- **Friends System**: Add friends and manage friend requests
- **User Search**: Find and connect with other users
- **Guest Mode**: Option to chat without registration

### User Interface
- Clean, modern, and responsive design
- Real-time message updates with typing indicators
- Separate views for friends, rooms, and direct messages
- Mobile-friendly interface with touch optimization
- Dark theme optimized for readability
- Modal overlays for user search and friend management

## 🛠️ Technologies

### Backend
- **Rust** - High-performance backend server
- **Axum** - Modern async web framework
- **SQLite** - Persistent data storage with SQLx
- **WebSockets** - Real-time communication
- **JWT** - Secure authentication tokens
- **bcrypt** - Password hashing
- **uuid** - Unique user identification

### Frontend
- **React 18** - Modern UI framework
- **Context API** - State management
- **Modern CSS** - Responsive styling
- **Vite** - Fast development and build tool

### Development
- **Bash scripts** - Development automation
- **Database migrations** - SQLx migration support
- **Hot reload** - Fast development iteration
- **Database initialization** - Automated setup scripts

## Prerequisites

- [Rust and Cargo](https://www.rust-lang.org/tools/install)
- [Node.js and npm](https://nodejs.org/en/download/)

## 🚀 Getting Started

The simplest way to get the development environment running is to use the provided shell script.

1. **Clone the repository** (if you haven't already).

2. **Make the development script executable:**
   ```bash
   chmod +x dev.sh
   ```

3. **Run the development script:**
   ```bash
   ./dev.sh
   ```

This command will automatically:
- Initialize SQLite database (if not exists)
- Install frontend dependencies (`npm install`) if they are not present
- Build the React frontend for production
- Start the Rust backend server with static file serving
- Serve the complete application at `http://localhost:3000`

Once the script is running, open your web browser and navigate to **http://localhost:3000** to use the application.

### Database Initialization

For first-time setup, initialize the database:

```bash
chmod +x init_db.sh
./init_db.sh
```

Or use the development script with database initialization:

```bash
./dev.sh --init-db
```

### Manual Installation

If you prefer to run the services separately during development:

#### Backend Server

```bash
# Navigate to the backend directory
cd backend

# Run the server (serves both API and static files)
cargo run
```

#### Frontend Development (Optional)

For frontend development with hot reload:

```bash
# Navigate to the frontend directory
cd frontend

# Install dependencies
npm install

# Run the development server
npm run dev
```

Then access the dev server at `http://localhost:5173` (will proxy API calls to backend).

## 🔑 Authentication System

### User Registration
- Navigate to the registration page
- Fill in username, email, and password (minimum 6 characters)
- Automatically logged in upon successful registration

### User Login
- Use existing credentials to log in
- JWT token stored securely in browser
- Automatic token validation on page refresh

### User Profile
- Click on user avatar to view profile
- See user information and account details
- Sign out functionality

### Guest Mode
- Chat without creating an account
- Limited to current session only (no message history)
- Can upgrade to full account anytime

### Friends & Direct Messages
- **Search users** by username
- **Send friend requests** to connect with others
- **Accept/decline** incoming friend requests
- **Start direct messages** with friends
- **See online status** of friends in real-time

### 📋 API Endpoints

### Authentication
- `POST /api/auth/register` - Register new user
- `POST /api/auth/login` - User login
- `POST /api/auth/verify` - Validate JWT token

### Chat Rooms & Messages
- `GET /api/rooms` - Get user's chat rooms (direct + group)
- `GET /api/rooms/:room_id/messages` - Get message history
- `WS /ws` - WebSocket connection for real-time chat

### Users & Friends
- `GET /api/users/search` - Search users by username
- `GET /api/friends` - Get user's friends list
- `GET /api/friends/requests` - Get pending friend requests
- `POST /api/friends/request` - Send friend request
- `POST /api/friends/accept` - Accept friend request

## 🧪 Testing

A test page is provided to verify the authentication system:

1. Open `test_auth.html` in your browser
2. Ensure the backend is running (`cargo run`)
3. Test registration, login, and token verification
4. View detailed API responses and error messages

## 🔧 Configuration

### Environment Variables

- `JWT_SECRET` - JWT signing secret (default: development key)
  ```bash
  export JWT_SECRET="your-secret-key-for-production"
  ```

- `DATABASE_URL` - Database connection string (default: sqlite:chatx.db)
  ```bash
  export DATABASE_URL="sqlite:chatx.db"
  ```

### Security Notes

- Change JWT_SECRET in production
- Use HTTPS in production
- Consider implementing rate limiting
- User data is stored persistently in SQLite database
- Passwords are hashed with bcrypt
- JWT tokens have automatic expiration

## 📂 Project Structure

```
.
├── backend/                 # Rust backend
│   ├── src/
│   │   ├── db/             # Database models and operations
│   │   │   └── mod.rs      # SQLite database implementation
│   │   └── main.rs         # Main server with enhanced APIs
│   ├── Cargo.toml          # Dependencies with database support
│   └── migrations/         # Database migrations
├── frontend/               # React frontend
│   ├── src/
│   │   ├── components/
│   │   │   ├── LoginPage.jsx        # Login interface
│   │   │   ├── RegisterPage.jsx     # Registration interface
│   │   │   ├── UserProfile.jsx      # User profile modal
│   │   │   ├── JoinForm.jsx         # Updated for room types
│   │   │   ├── FriendsList.jsx      # Friends management
│   │   │   ├── UserSearch.jsx       # User search component
│   │   │   ├── RoomsList.jsx        # Room browser
│   │   │   └── ...                  # Other chat components
│   │   ├── context/
│   │   │   └── AuthContext.jsx      # Enhanced authentication state
│   │   ├── App.jsx                  # Main app with new features
│   │   └── App.css                  # Updated styles
│   ├── package.json
│   └── index.html
├── dev.sh                  # Enhanced development script
├── init_db.sh              # Database initialization script
├── build.sh               # Production build script
├── clean.sh               # Clean build artifacts
├── chatx.db              # SQLite database (created automatically)
├── test_auth.html         # Authentication testing page
├── demo.md                # Feature demonstration guide
└── README.md              # This file
```

## 🚀 Deployment

### Production Build

```bash
# Initialize database (first time only)
./init_db.sh

# Build frontend for production
cd frontend && npm run build

# Run backend (serves built frontend)
cd ../backend && cargo run --release
```

### Environment Setup for Production

```bash
# Set production environment variables
export JWT_SECRET="your-production-secret-key"
export DATABASE_URL="sqlite:chatx.db"

# Run in production mode
cd backend && cargo run --release
```

### Docker (Optional)

Create a Dockerfile for containerized deployment:

```dockerfile
# Build frontend
FROM node:18 AS frontend
WORKDIR /app/frontend
COPY frontend/package*.json ./
RUN npm install
COPY frontend/ ./
RUN npm run build

# Build backend
FROM rust:1.70 AS backend
WORKDIR /app
COPY backend/ ./
RUN cargo build --release

# Final image
FROM debian:bullseye-slim
WORKDIR /app
COPY --from=backend /app/target/release/backend ./
COPY --from=frontend /app/frontend/dist ./frontend/dist
EXPOSE 3000
CMD ["./backend"]
```

## 🤝 Contributing

1. Fork the repository
2. Create your feature branch (`git checkout -b feature/AmazingFeature`)
3. Commit your changes (`git commit -m 'Add some AmazingFeature'`)
4. Push to the branch (`git push origin feature/AmazingFeature`)
5. Open a Pull Request

## 📝 License

This project is open source and available under the [MIT License](LICENSE).

## 🙏 Acknowledgments

- Built with assistance from Gemini AI
- Rust community for excellent crates
- React team for the amazing framework
- All contributors and users of this project