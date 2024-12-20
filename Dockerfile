# Build stage
FROM node:20-slim AS builder
WORKDIR /app

# Install dependencies
COPY package*.json ./
RUN npm ci

# Copy source and build
COPY . .
RUN npm run build

# Production stage
FROM node:20-alpine AS production
WORKDIR /app

# Copy only production dependencies
COPY package*.json ./
RUN npm ci --omit=dev

# Copy only the built files from builder
COPY --from=builder /app/build ./build

# Add tini for proper signal handling (alpine version)
RUN apk add --no-cache tini

# Set user for security
USER node

# Use tini as entrypoint
ENTRYPOINT ["/sbin/tini", "--"]

EXPOSE 3000
CMD ["node", "build/server.js"]