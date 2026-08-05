package ca

import (
	"crypto"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"

	"github.com/eclipse-keypont/crypto11"
)

type PKCS11Signer struct {
	context *crypto11.Context
	crypto.Signer
}

func LoadPKCS11Signer(configPath, keyID string) (*PKCS11Signer, error) {
	info, err := os.Lstat(filepath.Clean(configPath))
	if err != nil {
		return nil, fmt.Errorf("inspect PKCS#11 configuration: %w", err)
	}
	if !info.Mode().IsRegular() || info.Mode().Perm()&0o077 != 0 {
		return nil, errors.New("PKCS#11 configuration must be a regular owner-only file")
	}
	decodedID, err := hex.DecodeString(keyID)
	if err != nil || len(decodedID) == 0 {
		return nil, errors.New("PKCS#11 key ID must be non-empty hexadecimal")
	}
	context, err := crypto11.ConfigureFromFile(filepath.Clean(configPath))
	if err != nil {
		return nil, fmt.Errorf("configure PKCS#11 token: %w", err)
	}
	signer, err := context.FindKeyPair(decodedID, nil)
	if err != nil {
		context.Close()
		return nil, fmt.Errorf("find PKCS#11 CA key: %w", err)
	}
	if signer == nil {
		context.Close()
		return nil, errors.New("PKCS#11 CA key was not found")
	}
	return &PKCS11Signer{context: context, Signer: signer}, nil
}

func (signer *PKCS11Signer) Close() error {
	return signer.context.Close()
}
