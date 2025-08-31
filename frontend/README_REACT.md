# React Chat Frontend

This is a React-based frontend for the Rust Chat Application, built with Vite and modern React patterns.

## Features

- **Modern React Architecture**: Built with React 18, hooks, and functional components
- **Real-time Communication**: WebSocket integration for live chat
- **Responsive Design**: Mobile-friendly interface with modern CSS
- **Type Safety**: Full TypeScript support available
- **Developer Experience**: Hot reload, ESLint, and optimized build

## Project Structure

```
src/
├── components/          # React components
│   ├── JoinForm.jsx    # Join chat form
│   ├── ChatRoom.jsx    # Main chat interface
│   ├── MessageList.jsx # Message display component
│   ├── Message.jsx     # Individual message component
│   └── MessageInput.jsx# Message input component
├── App.jsx             # Main application component
├── App.css             # Application styles
├── main.jsx            # React entry point
└── style.css           # Base CSS styles
```

## Getting Started

### Prerequisites

- Node.js 16+ 
- npm or yarn

### Installation

1. Install dependencies:
```bash
npm install
```

2. Start development server:
```bash
npm run dev
```

3. Build for production:
```bash
npm run build
```

4. Preview production build:
```bash
npm run preview
```

## Configuration

The application connects to the WebSocket server at `ws://127.0.0.1:3000/ws` by default. To change this, modify the WebSocket URL in `src/App.jsx`.

## Available Scripts

- `npm run dev` - Start development server with hot reload
- `npm run build` - Build for production
- `npm run preview` - Preview production build locally
- `npm run lint` - Run ESLint for code quality

## Key Components

### JoinForm
Handles user authentication with username and channel selection.

### ChatRoom
Main chat interface with message display and user list.

### MessageList
Displays chat messages with system messages and user messages.

### Message
Renders individual chat messages with avatars and timestamps.

### MessageInput
Handles message composition and sending.

## Styling

The application uses CSS custom properties for theming and includes:
- Modern gradient backgrounds
- Glassmorphism effects
- Responsive design
- Smooth animations
- Accessibility features

## Backend Integration

The frontend expects the backend to provide WebSocket messages in the following format:

```json
{
  "message_type": "user_list" | "system" | "message",
  "username": "string",
  "message": "string",
  "channel": "string"
}
```

## Development

### Adding New Features
1. Create components in `src/components/`
2. Add styles to `src/App.css`
3. Update state management in `src/App.jsx`

### State Management
The app uses React's built-in state management with `useState` and `useEffect` hooks. For larger applications, consider integrating Zustand or Redux.

### Testing
To add tests, install testing libraries:
```bash
npm install --save-dev @testing-library/react @testing-library/jest-dom
```

## Deployment

### Vercel/Netlify
The build output is in the `dist/` directory, ready for deployment to static hosting services.

### Docker
Create a Dockerfile for containerized deployment:

```dockerfile
FROM node:18-alpine
WORKDIR /app
COPY package*.json ./
RUN npm ci --only=production
COPY . .
RUN npm run build
EXPOSE 4173
CMD ["npm", "run", "preview"]
```

## Troubleshooting

### Common Issues

1. **WebSocket Connection Failed**
   - Ensure backend server is running on port 3000
   - Check CORS settings on backend

2. **Build Errors**
   - Clear node_modules and reinstall dependencies
   - Check Node.js version compatibility

3. **Styling Issues**
   - Verify CSS custom properties are properly defined

## Contributing

1. Fork the repository
2. Create a feature branch
3. Make changes with proper testing
4. Submit a pull request

## License

This project is part of the Rust Chat Application. See main project for license details.