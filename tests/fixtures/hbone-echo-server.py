import socket
import sys

import h2.config
import h2.connection
import h2.events


listener = socket.create_server(("127.0.0.1", 15008))
connection_socket, _ = listener.accept()
connection = h2.connection.H2Connection(
    config=h2.config.H2Configuration(
        client_side=False,
        header_encoding="utf-8",
        validate_inbound_headers=False,
    )
)
connection.initiate_connection()
connection_socket.sendall(connection.data_to_send())
completed = False
payloads = {}
while True:
    data = connection_socket.recv(65535)
    if not data:
        sys.exit(0 if completed else "HBONE client closed before completing a stream")
    for event in connection.receive_data(data):
        if isinstance(event, h2.events.RequestReceived):
            headers = dict(event.headers)
            assert headers[":method"] == "CONNECT"
            assert headers[":authority"] == "203.0.113.7:443"
            payloads[event.stream_id] = bytearray()
            connection.send_headers(event.stream_id, [(":status", "200")])
        elif isinstance(event, h2.events.DataReceived):
            payloads[event.stream_id].extend(event.data)
            assert b"client-tls-bytes".startswith(payloads[event.stream_id])
            connection.acknowledge_received_data(event.flow_controlled_length, event.stream_id)
            if payloads[event.stream_id] == b"client-tls-bytes" and not completed:
                connection.send_data(event.stream_id, b"gateway-tls-bytes", end_stream=True)
                completed = True
    connection_socket.sendall(connection.data_to_send())