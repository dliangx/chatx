#!/bin/bash

# ChatX Database Initialization Script
# This script initializes the SQLite database for the chat application

set -e

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

# Default database path
DATABASE_PATH="chatx.db"
BACKEND_DIR="backend"

# Check if we're in the right directory
if [ ! -d "$BACKEND_DIR" ]; then
    print_error "Please run this script from the project root directory"
    exit 1
fi

# Check if sqlx-cli is installed
if ! command -v sqlx &> /dev/null; then
    print_status "sqlx-cli not found, installing..."
    cargo install sqlx-cli --no-default-features --features sqlite
fi

# Set database URL
export DATABASE_URL="sqlite:$DATABASE_PATH"

print_status "Initializing database at: $DATABASE_PATH"

# Create database file if it doesn't exist
if [ ! -f "$DATABASE_PATH" ]; then
    print_status "Creating new database file..."
    touch "$DATABASE_PATH"
fi

# Run database migrations
print_status "Running database migrations..."
cd "$BACKEND_DIR"

if sqlx database create; then
    print_success "Database created successfully"
else
    print_error "Failed to create database"
    exit 1
fi

if sqlx migrate run; then
    print_success "Migrations applied successfully"
else
    print_error "Failed to apply migrations"
    exit 1
fi

cd ..

print_status "Verifying database structure..."
if sqlx query "SELECT name FROM sqlite_master WHERE type='table';" --database-url "$DATABASE_URL" > /dev/null 2>&1; then
    print_success "Database structure verified"
else
    print_error "Database verification failed"
    exit 1
fi

# Display database info
print_status "Database information:"
echo "-----------------------"
sqlite3 "$DATABASE_PATH" ".tables"
echo "-----------------------"

print_success "Database initialization completed successfully!"
print_status "Database file: $DATABASE_PATH"
print_status "You can now start the application with: ./dev.sh"

# Set proper permissions
chmod 644 "$DATABASE_PATH"

exit 0
