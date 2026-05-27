# Legacy Terms

This terms are used through codebase and documentation. Their future used must be avoided.
They remain documented here so existing code can be read and migrated deliberately by replacing them with the terms defined in ./THESAURUS.md.

## MethodId
- **Definition**: Former legacy implementation name for Payment Endpoint Identifier.
- **NOT**: Canonical domain language.
- **Synonyms to AVOID**: MethodId in new domain-facing code/docs
- **Related terms**: Payment Endpoint Identifier

## EndpointData
- **Definition**: Former legacy implementation name for Payment Endpoint Payload.
- **NOT**: Canonical domain language.
- **Synonyms to AVOID**: EndpointData in new domain-facing code/docs
- **Related terms**: Payment Endpoint Payload

## SupportedPayments
- **Definition**: Former legacy implementation name for the payee-published Payment List.
- **NOT**: The payer-side post processing of Payment List.
- **Synonyms to AVOID**: SupportedPayments in new domain-facing code/docs
- **Related terms**: Payment List

## PrivatePaymentsPayload
- **Definition**: Former legacy implementation name for Private Payment Envelope.
- **NOT**: Canonical domain language.
- **Synonyms to AVOID**: PrivatePaymentsPayload, private payments payload
- **Related terms**: Private Payment Envelope

## Paykit PDK
- **Definition**: Legacy artifact/name for earlier Paykit library work.
- **NOT**: The canonical current component name.
- **Synonyms to AVOID**: Paykit PDK
- **Related terms**: Paykit Library

## Paykit FFI
- **Definition**: Implementation detail for bindings generated from Paykit Library.
- **NOT**: A first-class Paykit architecture component.
- **Synonyms to AVOID**: Paykit FFI as product/component language
- **Related terms**: Paykit Library, Language Bindings

## Paykit React Native
- **Definition**: Implementation/distribution detail under Paykit Library bindings.
- **NOT**: A first-class Paykit architecture component.
- **Synonyms to AVOID**: React Native Paykit as product/component language
- **Related terms**: Paykit Library, Language Bindings

