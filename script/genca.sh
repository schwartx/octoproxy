#!/bin/bash

# CA private key
openssl genrsa -out ca.key 2048

# CA cert
openssl req -new -x509 -key ca.key -out ca.crt
