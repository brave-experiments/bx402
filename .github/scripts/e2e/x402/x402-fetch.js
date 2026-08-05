// The paying half of the @x402/fetch leg. Reads the payer key and the URL from
// the environment, writes the response body where the shell leg asserts on it,
// and prints the status and receipt for the run's forensics.
import { writeFileSync } from 'node:fs';
import { decodePaymentResponseHeader, wrapFetchWithPayment, x402Client } from '@x402/fetch';
import { ExactEvmScheme } from '@x402/evm/exact/client';
import { privateKeyToAccount } from 'viem/accounts';

const account = privateKeyToAccount(process.env.PAYER_KEY);
// Registering Base Sepolia alone means the mainnet offer carried in the same 402
// is never chosen, whatever order the server lists the offers in.
const client = new x402Client().register('eip155:84532', new ExactEvmScheme(account));

const response = await wrapFetchWithPayment(fetch, client)(process.env.URL);
writeFileSync('response.txt', await response.text());
console.log(`status: ${response.status}`);

// Unlike purl, this client surfaces the settlement receipt, so the transaction
// is readable straight from the response.
const header = response.headers.get('payment-response');
if (header) {
  console.log(`receipt: ${JSON.stringify(decodePaymentResponseHeader(header))}`);
}

if (!response.ok) {
  process.exit(1);
}
