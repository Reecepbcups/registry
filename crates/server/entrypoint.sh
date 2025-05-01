#!/bin/bash
set -e

if [ -n "$WARG_OPERATOR_KEY" ]; then
    OPERATOR_KEY="--operator-key $WARG_OPERATOR_KEY"
else
    OPERATOR_KEY=""
fi

exec warg-server --content-dir "$CONTENT_DIR" ${OPERATOR_KEY}
