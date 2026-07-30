FROM node:24-slim AS builder
WORKDIR /app

# Instala dependências
COPY package.json package-lock.json ./
RUN npm ci

# Build da aplicação React
COPY . .
RUN npm run build

# Runtime nginx para servir SPA e fazer proxy de /api e /ws para o backend
FROM nginx:1.27-alpine

COPY --from=builder /app/dist /usr/share/nginx/html
COPY nginx.conf /etc/nginx/conf.d/default.conf

EXPOSE 80

CMD ["nginx", "-g", "daemon off;"]
