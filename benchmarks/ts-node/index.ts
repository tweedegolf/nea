import * as net from 'net';
import {Request} from "./types";

const PORT = 8001;
const handlerName = process.argv[2] || 'average';
const handlerModule = require(`./handlers/${handlerName}`);

const server = net.createServer((socket) => {
	socket.on('data', (data) => {
		const requestString = data.toString();

		// Basic parsing of the request
		const [requestLine, _] = requestString.split('\r\n');
		const [method, path, version] = requestLine.split(' ');
		const headers = new Map<string, string>();
		// TODO: Parse headers
		const bodyIndex = requestString.indexOf('\r\n\r\n');
		const body = requestString.substring(bodyIndex + 4);

		const request: Request = { body, headers, method, path, version };

		// Call the handler and get the response
		const { statusCode, contentType, body: responseBody } = handlerModule.handleRequest(request);

		// Format the response
		const response = `HTTP/1.1 ${statusCode}\r\nContent-Type: ${contentType}\r\nContent-Length: ${responseBody.length}\r\n\r\n${responseBody}`;

		// Send the response back to the client
		socket.write(response, () => {
			socket.end();
		});
	});
});

server.listen(PORT);
