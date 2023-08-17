import socket
import time

def send_tcp_request(host, port, message):
    try:
        # Create a socket object
        client_socket = socket.socket(socket.AF_INET, socket.SOCK_STREAM)

        # Connect to the server
        client_socket.connect((host, port))

        # Send the message
        client_socket.send(message.encode())

        # Receive and print the response
        response = client_socket.recv(1024)
        print(f"Received response: {response.decode()}")

        # Close the socket
        client_socket.close()

    except Exception as e:
        print(f"Error: {e}")

if __name__ == "__main__":
    host = "localhost"  # Change this to the server's IP address if not running locally
    port = 8000
    message = "xyz"

    try:
        while True:
            send_tcp_request(host, port, message)
            time.sleep(0.1)  # Wait for 10 seconds before sending the next request

    except KeyboardInterrupt:
        print("Program terminated.")

