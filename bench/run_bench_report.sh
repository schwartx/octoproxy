#!/bin/bash

bash ./bench_hey.sh -np | tee ./direct_report.out
bash ./bench_hey.sh | tee proxy_report.out
env ./venv/bin/python3 ./parse_hey_report.py
