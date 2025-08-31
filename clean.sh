#!/bin/bash

# Rust Chat App - Clean Build Script
# This script cleans build artifacts and temporary files

set -e  # Exit on any error

echo "🧹 Cleaning Rust Chat App..."

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

# Parse command line arguments
CLEAN_DEPS=false
CLEAN_LOGS=false
CLEAN_ALL=false

while [[ $# -gt 0 ]]; do
    case $1 in
        --deps)
            CLEAN_DEPS=true
            shift
            ;;
        --logs)
            CLEAN_LOGS=true
            shift
            ;;
        --all)
            CLEAN_ALL=true
            CLEAN_DEPS=true
            CLEAN_LOGS=true
            shift
            ;;
        --help|-h)
            echo "Usage: $0 [OPTIONS]"
            echo "Options:"
            echo "  --deps      Also remove node_modules and Cargo registry cache"
            echo "  --logs      Also remove log files"
            echo "  --all       Clean everything (deps + logs + build artifacts)"
            echo "  --help      Show this help message"
            exit 0
            ;;
        *)
            print_error "Unknown option: $1"
            echo "Use --help for usage information"
            exit 1
            ;;
    esac
done

# Check if we're in the right directory
if [ ! -d "backend" ] || [ ! -d "frontend" ]; then
    print_error "Please run this script from the project root directory"
    exit 1
fi

# Function to safely remove directory/file
safe_remove() {
    local path=$1
    local description=$2

    if [ -e "$path" ]; then
        print_status "Removing $description..."
        rm -rf "$path"
        print_success "Removed $description"
    fi
}

# Function to get directory size
get_size() {
    local path=$1
    if [ -e "$path" ]; then
        du -sh "$path" 2>/dev/null | cut -f1
    else
        echo "0B"
    fi
}

# Calculate sizes before cleaning
print_status "Calculating current disk usage..."

BACKEND_TARGET_SIZE=$(get_size "backend/target")
FRONTEND_DIST_SIZE=$(get_size "frontend/dist")
FRONTEND_NODE_MODULES_SIZE=$(get_size "frontend/node_modules")

echo ""
print_status "Current build artifacts:"
[ -d "backend/target" ] && echo "  Backend target/: $BACKEND_TARGET_SIZE"
[ -d "frontend/dist" ] && echo "  Frontend dist/: $FRONTEND_DIST_SIZE"
[ -d "frontend/node_modules" ] && echo "  Frontend node_modules/: $FRONTEND_NODE_MODULES_SIZE"
echo ""

# Clean backend build artifacts
print_status "Cleaning backend build artifacts..."
cd backend

if [ -d "target" ]; then
    cargo clean
    print_success "Backend build artifacts cleaned"
else
    print_warning "No backend build artifacts found"
fi

cd ..

# Clean frontend build artifacts
print_status "Cleaning frontend build artifacts..."
safe_remove "frontend/dist" "frontend dist directory"
safe_remove "frontend/.vite" "Vite cache"

# Clean temporary and cache files
print_status "Cleaning temporary files..."
safe_remove ".DS_Store" ".DS_Store files"
find . -name ".DS_Store" -delete 2>/dev/null || true
safe_remove "*.log" "log files in root"
safe_remove ".env.local" "local environment file"

# Clean dependencies if requested
if [ "$CLEAN_DEPS" = true ]; then
    print_status "Cleaning dependencies..."

    # Clean frontend dependencies
    safe_remove "frontend/node_modules" "frontend node_modules"
    safe_remove "frontend/package-lock.json" "frontend package-lock.json"

    # Clean Cargo registry cache (optional, as it's shared)
    print_warning "Note: Not cleaning Cargo registry cache as it's shared across projects"
    print_status "To clean Cargo cache manually, run: cargo clean --target-dir ~/.cargo/registry"
fi

# Clean log files if requested
if [ "$CLEAN_LOGS" = true ]; then
    print_status "Cleaning log files..."
    find . -name "*.log" -delete 2>/dev/null || true
    safe_remove "logs" "logs directory"
    safe_remove "backend/logs" "backend logs directory"
    safe_remove "frontend/logs" "frontend logs directory"
fi

# Clean additional development files
print_status "Cleaning development artifacts..."
safe_remove ".vscode/settings.json.bak" "VSCode settings backup"
safe_remove "*.swp" "Vim swap files"
safe_remove "*.swo" "Vim swap files"
safe_remove "*~" "backup files"

# Clean any editor temporary files
find . -name "*.tmp" -delete 2>/dev/null || true
find . -name "*.temp" -delete 2>/dev/null || true

# Summary
echo ""
print_success "🎉 Cleanup completed!"

# Show what would be needed to restore
echo ""
print_status "To restore the development environment:"

if [ "$CLEAN_DEPS" = true ]; then
    echo "  1. Run: cd frontend && npm install"
    echo "  2. Run: ./dev.sh (to start development servers)"
else
    echo "  1. Run: ./dev.sh (to start development servers)"
fi

echo "  Or run: ./build.sh (to build for production)"

# Show freed space (rough estimate)
echo ""
if [ "$CLEAN_DEPS" = true ]; then
    print_status "Approximate disk space freed: $BACKEND_TARGET_SIZE + $FRONTEND_DIST_SIZE + $FRONTEND_NODE_MODULES_SIZE"
else
    print_status "Approximate disk space freed: $BACKEND_TARGET_SIZE + $FRONTEND_DIST_SIZE"
fi
