package main

import (
	"context"
	"errors"
	"flag"
	"log"
	"net/http"
	"os"
	"os/signal"
	"syscall"
	"time"

	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/api"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/auth"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/ca"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/enrollment"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/internal/store/postgres"
	"github.com/agentdesktop-dev/agentdesktop/control-plane/migrations"
)

func main() {
	migrate := flag.Bool("migrate", false, "apply pending database migrations before startup")
	flag.Parse()

	databaseURL := required("DATABASE_URL")
	issuer := required("OAUTH_ISSUER")
	audience := required("OAUTH_AUDIENCE")
	scope := required("OAUTH_SCOPE")
	administratorScope := required("ADMIN_OAUTH_SCOPE")
	organizationID := required("ORGANIZATION_ID")
	organizationName := required("ORGANIZATION_NAME")
	caCertificatePath := required("CA_CERTIFICATE_PATH")
	caKeyPath := required("CA_PRIVATE_KEY_PATH")
	trustDomain := required("MTLS_TRUST_DOMAIN")
	listenAddress := value("LISTEN_ADDRESS", "127.0.0.1:8090")
	certificateLifetime, err := time.ParseDuration(value("CLIENT_CERTIFICATE_LIFETIME", "24h"))
	if err != nil {
		log.Fatalf("invalid CLIENT_CERTIFICATE_LIFETIME: %v", err)
	}

	ctx, stop := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer stop()
	if *migrate {
		if err := migrations.Apply(ctx, databaseURL); err != nil {
			log.Fatal(err)
		}
	}
	store, err := postgres.Open(ctx, databaseURL)
	if err != nil {
		log.Fatal(err)
	}
	defer store.Close()
	if err := store.EnsureOrganization(ctx, organizationID, issuer, organizationName); err != nil {
		log.Fatal(err)
	}
	client := &http.Client{Timeout: 10 * time.Second}
	validator, err := auth.Discover(ctx, client, issuer, audience, scope)
	if err != nil {
		log.Fatal(err)
	}
	administratorValidator, err := auth.Discover(ctx, client, issuer, audience, administratorScope)
	if err != nil {
		log.Fatal(err)
	}
	certificateIssuer, err := ca.LoadX509Issuer(
		caCertificatePath,
		caKeyPath,
		trustDomain,
		certificateLifetime,
	)
	if err != nil {
		log.Fatal(err)
	}
	handler := api.NewServer(
		validator,
		administratorValidator,
		enrollment.NewService(store, certificateIssuer),
	)
	server := &http.Server{
		Addr:              listenAddress,
		Handler:           handler,
		ReadHeaderTimeout: 5 * time.Second,
		ReadTimeout:       15 * time.Second,
		WriteTimeout:      15 * time.Second,
		IdleTimeout:       60 * time.Second,
	}

	go func() {
		<-ctx.Done()
		shutdownContext, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		defer cancel()
		if err := server.Shutdown(shutdownContext); err != nil {
			log.Printf("server shutdown failed: %v", err)
		}
	}()
	log.Printf("enrollment server listening on %s", listenAddress)
	if err := server.ListenAndServe(); err != nil && !errors.Is(err, http.ErrServerClosed) {
		log.Fatal(err)
	}
}

func required(name string) string {
	value := os.Getenv(name)
	if value == "" {
		log.Fatalf("required environment variable %s is unset", name)
	}
	return value
}

func value(name, fallback string) string {
	if configured := os.Getenv(name); configured != "" {
		return configured
	}
	return fallback
}
