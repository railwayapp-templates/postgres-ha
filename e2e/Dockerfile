FROM node:20-slim

WORKDIR /app

COPY e2e/package*.json ./
RUN npm ci

COPY e2e/tsconfig.json ./
COPY e2e/src ./src

CMD ["npm", "test"]
