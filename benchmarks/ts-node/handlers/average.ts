import { Request, Response } from '../types';

export const handleRequest = async (request: Request): Promise<Response> => {
	const lines = request.body.split('\n');
	let path = 'M 0 0 L';

	for (const line of lines) {
		const [xStr, yStr] = line.split(', ');
		if (!xStr || !yStr) {
			throw new Error('Invalid input');
		}
		const x = parseInt(xStr, 10);
		const y = parseInt(yStr, 10);
		if (isNaN(x) || isNaN(y)) {
			throw new Error('Invalid input');
		}
		path += `${x} ${y} `;
	}

	const svg = `<svg width="100" height="100" xmlns="http://www.w3.org/2000/svg">
        <path d="${path.trim()}" stroke="black" fill="transparent"/>
    </svg>`;

	return {
		statusCode: '200 OK',
		contentType: 'image/svg+xml',
		body: svg
	};
};
