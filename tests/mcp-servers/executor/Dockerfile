FROM node:26.12.1-bookworm-slim
LABEL caudra-mcp-it=""
ENV EXECUTOR_DISABLE_ANALYTICS=1 \
    EXECUTOR_DISABLE_UPDATE_CHECK=1 \
    EXECUTOR_DISABLE_INTEGRATIONS_FETCH=1
RUN apt-get update \
    && apt-get upgrade --yes --no-install-recommends \
    && rm -rf /var/lib/apt/lists/*
RUN npm install --global npm@12.2.1 \
    && npm install --global executor@1.6.11 \
    && npm cache clean --force
COPY --chmod=755 start.sh /usr/local/bin/start-executor
ENTRYPOINT ["start-executor"]
