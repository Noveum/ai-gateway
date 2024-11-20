# Deployment Guide

## Deployment Options

### Docker Deployment

#### Using Docker Compose
```yaml
version: '3.8'
services:
  magicapi:
    image: magicapi/ai-gateway:latest
    ports:
      - "3000:3000"
    environment:
      - RUST_LOG=info
      - PORT=3000
```

#### Manual Docker Commands
```bash
# Build the image
docker build -t magicapi-gateway .

# Run the container
docker run -d \
  -p 3000:3000 \
  -e RUST_LOG=info \
  --name magicapi \
  magicapi-gateway
```

### Kubernetes Deployment

```yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: magicapi-gateway
spec:
  replicas: 3
  selector:
    matchLabels:
      app: magicapi-gateway
  template:
    metadata:
      labels:
        app: magicapi-gateway
    spec:
      securityContext:
        runAsNonRoot: true
        runAsUser: 1000
      containers:
      - name: magicapi-gateway
        image: magicapi/ai-gateway:latest
        ports:
        - containerPort: 3000
        env:
        - name: RUST_LOG
          value: "info"
        resources:
          limits:
            cpu: "1"
            memory: "1Gi"
          requests:
            cpu: "500m"
            memory: "512Mi"
        livenessProbe:
          httpGet:
            path: /health
            port: 3000
          initialDelaySeconds: 5
          periodSeconds: 10
        readinessProbe:
          httpGet:
            path: /ready
            port: 3000
          initialDelaySeconds: 5
          periodSeconds: 10
```

+```yaml
+apiVersion: v1
+kind: Service
+metadata:
+  name: magicapi-gateway
+spec:
+  selector:
+    app: magicapi-gateway
+  ports:
+    - protocol: TCP
+      port: 80
+      targetPort: 3000
+  type: ClusterIP
+```

## Production Considerations

### Security Checklist
- [ ] Enable TLS/SSL
- [ ] Configure proper CORS settings
- [ ] Set up API key authentication
- [ ] Enable rate limiting
- [ ] Configure proper logging
- [ ] Set up monitoring and alerts
- [ ] Configure AWS IAM roles with least privilege
- [ ] Enable AWS CloudTrail for API activity logging
- [ ] Set up AWS VPC endpoints for Bedrock
- [ ] Configure AWS KMS for encryption
- [ ] Implement AWS WAF rules


## Scaling Strategies

### Horizontal Scaling
```bash
# Kubernetes scaling
kubectl scale deployment magicapi-gateway --replicas=5

# Docker Swarm scaling
docker service scale magicapi_gateway=5
```

### Resource Allocation
```yaml
resources:
  limits:
    cpu: "2"
    memory: "2Gi"
  requests:
    cpu: "1"
    memory: "1Gi"
```

## Maintenance

### Update Procedure
```bash
# Rolling update in Kubernetes
kubectl set image deployment/magicapi-gateway \
  magicapi-gateway=magicapi/ai-gateway:new-version

# Docker update
docker-compose pull
docker-compose up -d
```

### Health Checks
```yaml
livenessProbe:
  httpGet:
    path: /health
    port: 3000
  initialDelaySeconds: 3
  periodSeconds: 3
``` 