#!/bin/bash

# Rust Chat App - Development Script
# This script runs both frontend and backend in development mode

set -e  # Exit on any error

echo "🚀 Starting Development Environment..."

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# Function to print colored output
print_status() {
    echo -e "${BLUE}[INFO]${NC} $1"
}

print_success() {
    echo -e "${GREEN}[SUCCESS]${NC} $1"
}

print_warning() {
    echo -e "${YELLOW}[WARNING]${NC} $1"
}

print_error() {
    echo -e "${RED}[ERROR]${NC} $1"
}

# Function to kill background processes on exit
cleanup() {
    echo ""
    print_status "Shutting down development servers..."

    # Kill all background jobs
    if [ -n "$(jobs -p)" ]; then
        kill $(jobs -p) 2>/dev/null || true
    fi

    # Kill any remaining processes on the ports we use
    pkill -f "cargo run" 2>/dev/null || true
    pkill -f "vite" 2>/dev/null || true

    print_success "Development servers stopped"
    exit 0
}

# Set up signal handlers
trap cleanup SIGINT SIGTERM

# Check if we're in the right directory
if [ ! -d "backend" ] || [ ! -d "frontend" ]; then
    print_error "Please run this script from the project root directory"
    exit 1
fi

# Parse command line arguments
BACKEND_ONLY=false
FRONTEND_ONLY=false
BACKEND_PORT=3000
FRONTEND_PORT=5173

while [[ $# -gt 0 ]]; do
    case $1 in
        --backend-only)
            BACKEND_ONLY=true
            shift
            ;;
        --frontend-only)
            FRONTEND_ONLY=true
            shift
            ;;
        --backend-port)
            BACKEND_PORT="$2"
            shift 2
            ;;
        --frontend-port)
            FRONTEND_PORT="$2"
            shift 2
            ;;
        --help|-h)
            echo "Usage: $0 [OPTIONS]"
            echo "Options:"
            echo "  --backend-only      Run only the backend server"
            echo "  --frontend-only     Run only the frontend dev server"
            echo "  --backend-port PORT Set backend port (default: 8080)"
            echo "  --frontend-port PORT Set frontend port (default: 5173)"
            echo "  --help              Show this help message"
            exit 0
            ;;
        *)
            print_error "Unknown option: $1"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

# Check if frontend/dist exists, if not build frontend
if [ ! -d "frontend/dist" ]; then
    print_status "frontend/dist directory not found, building frontend..."
    cd frontend
    npm run build

    if [ $? -eq 0 ]; then
        print_success "Frontend build completed"
    else
        print_error "Frontend build failed"
        exit 1
    fi

    cd ..
else
    print_status "frontend/dist directory found, skipping build"
fi

# Start backend if not frontend-only
if [ "$FRONTEND_ONLY" != true ]; then
    print_status "Starting backend server on port $BACKEND_PORT..."

    # Check if backend directory and files exist
    if [ ! -f "backend/Cargo.toml" ]; then
        print_error "Backend Cargo.toml not found"
        exit 1
    fi

    cd backend

    # Set the port environment variable if specified
    if [ "$BACKEND_PORT" != "3000" ]; then
        export PORT=$BACKEND_PORT
    fi

    # Start backend in background
    cargo run &
    BACKEND_PID=$!

    cd ..

    print_success "Backend server started (PID: $BACKEND_PID)"

    # Give backend a moment to start
    sleep 2
fi

# Start frontend if not backend-only
if [ "$BACKEND_ONLY" != true ]; then
    print_status "Starting frontend dev server on port $FRONTEND_PORT..."

    # Check if frontend directory and files exist
    if [ ! -f "frontend/package.json" ]; then
        print_error "Frontend package.json not found"
        exit 1
    fi

    cd frontend

    # Install dependencies if node_modules doesn't exist
    if [ ! -d "node_modules" ]; then
        print_warning "node_modules not found, running npm install..."
        npm install

        if [ $? -ne 0 ]; then
            print_error "npm install failed"
            exit 1
        fi
    fi

    # Start frontend in background
    if [ "$FRONTEND_PORT" != "5173" ]; then
        npm run dev -- --port $FRONTEND_PORT &
    else
        npm run dev &
    fi

    FRONTEND_PID=$!

    cd ..

    print_success "Frontend dev server started (PID: $FRONTEND_PID)"
fi

# Show running services
echo ""
print_success "🎉 Development environment is ready!"
echo ""

if [ "$FRONTEND_ONLY" != true ]; then
    print_status "Backend API: http://localhost:$BACKEND_PORT"
fi

if [ "$BACKEND_ONLY" != true ]; then
    print_status "Frontend: http://localhost:$FRONTEND_PORT"
fi

echo ""
print_status "Press Ctrl+C to stop all servers"

# Wait for user to stop the servers
while true; do
    sleep 1
done
