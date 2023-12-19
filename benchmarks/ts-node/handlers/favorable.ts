import { Request, Response } from '../types';

export const handleRequest = async (request: Request): Promise<Response> => {
	const [_, after] = request.path.split('/', 2);
	const capacity = parseInt(after, 10) * 1024;

	const x = new Array(capacity);

	return {
		statusCode: '200 OK',
		contentType: 'text/plain',
		body: `${x.length}`
	};
};
