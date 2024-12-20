#!/bin/bash

# Get the version from package.json
VERSION=$(node -p "require('./package.json').version")
IMAGE_NAME="noveum/ai-gateway"

echo "Building Docker image v${VERSION}..."

# Build the Docker image with version tag
docker build -t ${IMAGE_NAME}:${VERSION} -t ${IMAGE_NAME}:latest .

if [ $? -eq 0 ]; then
    echo "Successfully built Docker images:"
    echo "  - ${IMAGE_NAME}:${VERSION}"
    echo "  - ${IMAGE_NAME}:latest"
    
    echo "To push to Docker Hub, run:"
    echo "  docker push ${IMAGE_NAME}:${VERSION}"
    echo "  docker push ${IMAGE_NAME}:latest"
else
    echo "Failed to build Docker image"
    exit 1
fi 