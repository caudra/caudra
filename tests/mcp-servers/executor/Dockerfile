FROM node:26.12.0-bookworm-slim
LABEL caudra-mcp-it=""
ENV EXECUTOR_DISABLE_ANALYTICS=1 \
    EXECUTOR_DISABLE_UPDATE_CHECK=1 \
    EXECUTOR_DISABLE_INTEGRATIONS_FETCH=1
RUN npm install --global npm@12.2.0 \
    && npm install --global executor@1.6.10 \
    && npm cache clean --force
COPY --chmod=755 start.sh /usr/local/bin/start-executor
ENTRYPOINT ["start-executor"]
