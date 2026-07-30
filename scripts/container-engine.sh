if [[ -n "${CONTAINER_ENGINE:-}" ]]; then
	case "$CONTAINER_ENGINE" in
		podman | docker)
			if ! command -v "$CONTAINER_ENGINE" >/dev/null 2>&1; then
				echo "$CONTAINER_ENGINE is not installed" >&2
				exit 1
			fi
			container_engine="$CONTAINER_ENGINE"
			;;
		*)
			echo "CONTAINER_ENGINE must be podman or docker" >&2
			exit 1
			;;
	esac
elif command -v podman >/dev/null 2>&1; then
	container_engine="podman"
elif command -v docker >/dev/null 2>&1; then
	container_engine="docker"
else
	echo "Podman or Docker is required" >&2
	exit 1
fi

readonly container_engine