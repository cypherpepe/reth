//! API of a signed transaction, w.r.t. network stack.

use alloy_consensus::TxLegacy;
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{keccak256, Address, Signature, TxHash, B256};

use crate::{transaction::decode_with_eip155_chain_id, Transaction};

/// A signed transaction.
pub trait SignedTransaction: Sized {
    /// Recover signer from signature and hash.
    ///
    /// Returns `None` if the transaction's signature is invalid following [EIP-2](https://eips.ethereum.org/EIPS/eip-2), see also [`recover_signer`](crate::transaction::recover_signer).
    ///
    /// Note:
    ///
    /// This can fail for some early ethereum mainnet transactions pre EIP-2, use
    /// [`Self::recover_signer_unchecked`] if you want to recover the signer without ensuring that
    /// the signature has a low `s` value.
    fn recover_signer(&self) -> Option<Address>;

    /// Recover signer from signature and hash _without ensuring that the signature has a low `s`
    /// value_.
    ///
    /// Returns `None` if the transaction's signature is invalid, see also
    /// [`recover_signer_unchecked`](crate::transaction::recover_signer_unchecked).
    fn recover_signer_unchecked(&self) -> Option<Address>;

    /// Output the length of the `encode_inner(out`, true). Note to assume that `with_header` is
    /// only `true`.
    fn payload_len_inner(&self) -> usize;

    /// Decodes an enveloped EIP-2718 typed transaction.
    ///
    /// This should _only_ be used internally in general transaction decoding methods,
    /// which have already ensured that the input is a typed transaction with the following format:
    /// `tx-type || rlp(tx-data)`
    ///
    /// Note that this format does not start with any RLP header, and instead starts with a single
    /// byte indicating the transaction type.
    ///
    /// CAUTION: this expects that `data` is `tx-type || rlp(tx-data)`
    fn decode_enveloped_typed_transaction(data: &mut &[u8]) -> alloy_rlp::Result<Self>;

    /// Returns the length without an RLP header - this is used for eth/68 sizes.
    fn length_without_header(&self) -> usize;

    /// Decodes legacy transaction from the data buffer.
    ///
    /// This should be used _only_ be used in general transaction decoding methods, which have
    /// already ensured that the input is a legacy transaction with the following format:
    /// `rlp(legacy_tx)`
    ///
    /// Legacy transactions are encoded as lists, so the input should start with a RLP list header.
    ///
    /// This expects `rlp(legacy_tx)`
    // TODO: make buf advancement semantics consistent with `decode_enveloped_typed_transaction`,
    // so decoding methods do not need to manually advance the buffer
    fn decode_rlp_legacy_transaction(data: &mut &[u8]) -> alloy_rlp::Result<Self>;

    /// Decodes legacy transaction from the data buffer into a tuple.
    ///
    /// This expects `rlp(legacy_tx)`
    ///
    /// Refer to the docs for [`Self::decode_rlp_legacy_transaction`] for details on the exact
    /// format expected.
    fn decode_rlp_legacy_transaction_tuple(
        data: &mut &[u8],
    ) -> alloy_rlp::Result<(TxLegacy, TxHash, Signature)> {
        // keep this around, so we can use it to calculate the hash
        let original_encoding = *data;

        let header = alloy_rlp::Header::decode(data)?;
        let remaining_len = data.len();

        let transaction_payload_len = header.payload_length;

        if transaction_payload_len > remaining_len {
            return Err(alloy_rlp::Error::InputTooShort)
        }

        let mut transaction = TxLegacy {
            nonce: alloy_rlp::Decodable::decode(data)?,
            gas_price: alloy_rlp::Decodable::decode(data)?,
            gas_limit: alloy_rlp::Decodable::decode(data)?,
            to: alloy_rlp::Decodable::decode(data)?,
            value: alloy_rlp::Decodable::decode(data)?,
            input: alloy_rlp::Decodable::decode(data)?,
            chain_id: None,
        };
        let (signature, extracted_id) = decode_with_eip155_chain_id(data)?;
        transaction.chain_id = extracted_id;

        // check the new length, compared to the original length and the header length
        let decoded = remaining_len - data.len();
        if decoded != transaction_payload_len {
            return Err(alloy_rlp::Error::UnexpectedLength)
        }

        let tx_length = header.payload_length + header.length();
        let hash = keccak256(&original_encoding[..tx_length]);
        Ok((transaction, hash, signature))
    }

    /// Create a new signed transaction from a transaction and its signature.
    ///
    /// This will also calculate the transaction hash using its encoding.
    fn from_transaction_and_signature(transaction: Transaction, signature: Signature) -> Self;

    /// Calculate transaction hash, eip2728 transaction does not contain rlp header and start with
    /// tx type.
    fn recalculate_hash(&self) -> B256
    where
        Self: Encodable2718,
    {
        keccak256(self.encoded_2718())
    }
}