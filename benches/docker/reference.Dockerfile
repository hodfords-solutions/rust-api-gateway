# The Reference Gateway with the benchmark middleware. Build context: the repository root.
FROM node:24.20-slim
RUN npm install -g pnpm@12.1.0
WORKDIR /app
COPY benches/reference/package.json benches/reference/pnpm-lock.yaml benches/reference/pnpm-workspace.yaml benches/reference/tsconfig.json ./
RUN pnpm install --frozen-lockfile
COPY benches/reference/src src
RUN pnpm build && pnpm prune --prod
ENV NODE_ENV=production
CMD ["node", "dist/main.js"]
