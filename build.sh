#!/bin/bash

# Rust Chat App Build Script
# This script builds both frontend and backend components

set -e  # Exit on any error

echo "🚀 Building Rust Chat App..."

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

# Check if we're in the right directory
if [ ! -d "backend" ] || [ ! -d "frontend" ]; then
    print_error "Please run this script from the project root directory"
    exit 1
fi

# Build frontend
print_status "Building frontend..."
cd frontend

# Check if node_modules exists, if not run npm install
if [ ! -d "node_modules" ]; then
    print_warning "node_modules not found, running npm install..."
    npm install
fi

# Build frontend
npm run build

if [ $? -eq 0 ]; then
    print_success "Frontend build completed"
else
    print_error "Frontend build failed"
    exit 1
fi

cd ..

# Build backend
print_status "Building backend..."
cd backend

# Build backend in release mode
cargo build --release

if [ $? -eq 0 ]; then
    print_success "Backend build completed"
else
    print_error "Backend build failed"
    exit 1
fi

cd ..

print_success "🎉 All components built successfully!"
print_status "Frontend dist files are in: ./frontend/dist/"
print_status "Backend executable is in: ./backend/target/release/backend"
