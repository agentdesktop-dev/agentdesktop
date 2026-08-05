FROM golang:1.25.8-trixie AS build
WORKDIR /src
COPY go.mod go.sum ./
RUN go mod download
COPY . .
RUN CGO_ENABLED=1 go build -trimpath -ldflags='-s -w' -o /out/enrollment-server ./cmd/enrollment-server

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /out/enrollment-server /usr/local/bin/enrollment-server
ENTRYPOINT ["/usr/local/bin/enrollment-server"]