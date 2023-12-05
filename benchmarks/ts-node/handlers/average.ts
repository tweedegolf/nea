import { Request, Response } from '../types';

export const handleRequest = (request: Request): Response => {
	return {
		statusCode: '200 OK',
		contentType: 'text/plain',
		body: 'Hello, World!'
	};
};
