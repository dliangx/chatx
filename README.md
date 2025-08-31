# Rust Chat App

A simple real-time chat application built with a Rust backend and a vanilla JavaScript frontend with Vite.

This project was bootstrapped and enhanced with the help of a Gemini agent.

## ✨ Features

- Real-time messaging with WebSockets.
- Create or join different chat channels.
- A clean, modern, and responsive user interface.

## 🛠️ Technologies

-   **Backend**: Rust
-   **Frontend**: HTML5, CSS3, Vanilla JavaScript, Vite
-   **Development**: Bash script for concurrent execution

## Prerequisites

-   [Rust and Cargo](https://www.rust-lang.org/tools/install)
-   [Node.js and npm](https://nodejs.org/en/download/)

## 🚀 Getting Started

The simplest way to get the development environment running is to use the provided shell script.

1.  **Clone the repository** (if you haven't already).

2.  **Make the development script executable:**
    ```bash
    chmod +x dev.sh
    ```

3.  **Run the development script:**
    ```bash
    ./dev.sh
    ```

This command will automatically:
-   Install frontend dependencies (`npm install`) if they are not present.
-   Start the Rust backend server (available at `http://localhost:3000`).
-   Start the Vite frontend dev server (available at `http://localhost:5173`).

Once the script is running, open your web browser and navigate to **http://localhost:5173** to use the application.

### Manual Installation

If you prefer to run the services separately, follow these steps:

#### Backend Server

```bash
# Navigate to the backend directory
cd backend

# Run the server
cargo run
```

#### Frontend App

```bash
# Navigate to the frontend directory
cd frontend

# Install dependencies
npm install

# Run the development server
npm run dev
```

## 📂 Project Structure

```
.
├── backend/         # Rust backend source code
│   ├── src/
│   └── Cargo.toml
├── frontend/        # Frontend source code (Vite)
│   ├── src/
│   ├── index.html
│   └── package.json
├── dev.sh           # Main development startup script
├── build.sh         # Build script
├── clean.sh         # Script to clean build artifacts
└── README.md        # This file
```
